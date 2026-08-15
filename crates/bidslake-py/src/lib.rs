//! PyO3 bindings for querying a bidslake DuckDB database from Python.
//!
//! This crate owns the DuckDB connection (via the bundled engine in the
//! `duckdb` crate, the same one `bidslake` writes with) and hands query results
//! to Python as **Arrow IPC bytes**, so the Python package needs no `duckdb`
//! engine of its own and stays version-decoupled. The Python layer wraps these
//! primitives with the typed query surface (see `python/bidslake`).
//!
//! The MVP bridge serializes results to an Arrow IPC *stream* and returns the
//! bytes; Polars reads them with `pl.read_ipc_stream`. This is safe (no unsafe
//! FFI) and pyarrow-free. A later pass can switch to a zero-copy PyCapsule
//! stream if profiling of large results calls for it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use anyhow::Context;
use arrow::ipc::writer::StreamWriter;
use duckdb::arrow::record_batch::RecordBatch;
use duckdb::types::Value as DuckValue;
use duckdb::{AccessMode, Config, Connection, params_from_iter};
use pyo3::exceptions::{PyRuntimeError, PyTypeError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyBytes};

/// A read (or read-write) handle to a bidslake DuckDB database.
///
/// Holds the connection behind a `Mutex` so the `#[pyclass]` is `Send + Sync`
/// (a `duckdb::Connection` is `Send` but not `Sync`). The `Option` lets
/// [`PyLake::close`] drop the connection deterministically — releasing its file
/// handle and any write lock — instead of waiting for garbage collection.
#[pyclass(module = "bidslake._bidslake")]
struct PyLake {
    conn: Mutex<Option<Connection>>,
}

#[pymethods]
impl PyLake {
    /// Open the database at `path`. Read-only by default (the query product
    /// never mutates the catalog; also avoids contending with a writer).
    #[new]
    #[pyo3(signature = (path, read_only = true))]
    fn new(path: &str, read_only: bool) -> PyResult<Self> {
        let mode = if read_only {
            AccessMode::ReadOnly
        } else {
            AccessMode::ReadWrite
        };
        let config = Config::default()
            .access_mode(mode)
            .map_err(duck)
            .context("configuring DuckDB access mode")
            .map_err(anyhow_err)?;
        let conn = Connection::open_with_flags(path, config)
            .map_err(duck)
            .with_context(|| format!("opening DuckDB database at {path}"))
            .map_err(anyhow_err)?;
        Ok(PyLake {
            conn: Mutex::new(Some(conn)),
        })
    }

    /// Close the underlying DuckDB connection, releasing its file handle and (for a
    /// read-write handle) its write lock. Idempotent; any later query raises
    /// `RuntimeError`.
    fn close(&self) {
        *self.conn.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Run `sql` (with optional positional bind `params`) and return the result
    /// set as Arrow IPC stream bytes. Empty result sets still carry the schema,
    /// so the Python side gets correctly-typed (zero-row) frames.
    #[pyo3(signature = (sql, params = None))]
    fn query_ipc<'py>(
        &self,
        py: Python<'py>,
        sql: &str,
        params: Option<Vec<Bound<'py, PyAny>>>,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let values: Vec<DuckValue> = params
            .unwrap_or_default()
            .iter()
            .map(py_to_duck_value)
            .collect::<PyResult<_>>()?;
        // Detach from the GIL for the DuckDB prepare/execute/collect + Arrow-IPC
        // serialize: `query_ipc_bytes` touches no Python objects, so a long
        // query no longer stalls every other Python thread.
        let bytes = py
            .detach(|| self.query_ipc_bytes(sql, &values))
            .map_err(anyhow_err)?;
        Ok(PyBytes::new(py, &bytes))
    }

    /// Base tables and views in the `main` schema, sorted.
    fn list_tables(&self) -> PyResult<Vec<String>> {
        let guard = self.locked_conn();
        let conn = guard.as_ref().ok_or_else(closed)?;
        let mut stmt = conn
            .prepare(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = 'main' ORDER BY table_name",
            )
            .map_err(dberr("listing tables"))?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(dberr("listing tables"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(dberr("listing tables"))
    }

    /// The `(column_name, data_type)` pairs of `table`, in ordinal order.
    /// Includes the generated virtual columns (they are real catalog columns).
    fn columns(&self, table: &str) -> PyResult<Vec<(String, String)>> {
        let guard = self.locked_conn();
        let conn = guard.as_ref().ok_or_else(closed)?;
        let mut stmt = conn
            .prepare(
                "SELECT column_name, data_type FROM information_schema.columns \
                 WHERE table_schema = 'main' AND table_name = ? \
                 ORDER BY ordinal_position",
            )
            .map_err(dberr(format!("reading columns of {table}")))?;
        let rows = stmt
            .query_map([table], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(dberr(format!("reading columns of {table}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(dberr(format!("reading columns of {table}")))
    }

    /// The `bidslake_meta` version stamp `(schema_version, bids_version,
    /// bidslake_version)`, or `None` if the DB predates the stamp.
    fn meta(&self) -> PyResult<Option<(String, String, String)>> {
        let guard = self.locked_conn();
        let conn = guard.as_ref().ok_or_else(closed)?;
        let mut stmt = match conn.prepare(
            "SELECT schema_version, bids_version, bidslake_version \
             FROM bidslake_meta LIMIT 1",
        ) {
            Ok(stmt) => stmt,
            // Table absent (older DB) or any prepare error → treat as unstamped.
            Err(_) => return Ok(None),
        };
        let row = stmt.query_row([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        });
        match row {
            Ok(v) => Ok(Some(v)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(dberr("reading bidslake_meta")(e)),
        }
    }

    /// The full effective (base + overlays) schema JSON stamped into a database built
    /// with schema overlays, or `None` if the DB carries no augmentation (it was
    /// indexed without overlays, or predates the stamp). The stubgen reads this to
    /// generate static types for augmented entities/suffixes/columns.
    fn effective_schema(&self) -> PyResult<Option<String>> {
        let guard = self.locked_conn();
        let conn = guard.as_ref().ok_or_else(closed)?;
        let mut stmt =
            match conn.prepare("SELECT effective_schema::VARCHAR FROM bidslake_schema LIMIT 1") {
                Ok(stmt) => stmt,
                // Table absent (un-augmented or older DB) → no stored effective schema.
                Err(_) => return Ok(None),
            };
        match stmt.query_row([], |r| r.get::<_, String>(0)) {
            Ok(v) => Ok(Some(v)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(dberr("reading bidslake_schema")(e)),
        }
    }

    /// The applied overlays' provenance `(idx, source, sha256)` in application order,
    /// or an empty list if the DB carries no augmentation.
    fn overlays(&self) -> PyResult<Vec<(i64, String, String)>> {
        let guard = self.locked_conn();
        let conn = guard.as_ref().ok_or_else(closed)?;
        let mut stmt =
            match conn.prepare("SELECT idx, source, sha256 FROM bidslake_overlays ORDER BY idx") {
                Ok(stmt) => stmt,
                Err(_) => return Ok(Vec::new()),
            };
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i32>(0)? as i64,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(dberr("reading bidslake_overlays"))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(dberr("reading bidslake_overlays"))
    }

    /// The applied term maps' provenance `(idx, source, sha256)` in application order, or an
    /// empty list if the DB was indexed without any (mirrors [`overlays`]).
    fn term_maps(&self) -> PyResult<Vec<(i64, String, String)>> {
        self.provenance("bidslake_term_maps")
    }

    /// The applied ingestion fragments' provenance `(idx, source, sha256)`.
    fn ingestion(&self) -> PyResult<Vec<(i64, String, String)>> {
        self.provenance("bidslake_ingestion")
    }
}

impl PyLake {
    /// Read a `bidslake_<kind>` provenance table (empty if absent).
    fn provenance(&self, table: &str) -> PyResult<Vec<(i64, String, String)>> {
        let guard = self.locked_conn();
        let conn = guard.as_ref().ok_or_else(closed)?;
        let sql = format!("SELECT idx, source, sha256 FROM {table} ORDER BY idx");
        let mut stmt = match conn.prepare(&sql) {
            Ok(stmt) => stmt,
            Err(_) => return Ok(Vec::new()),
        };
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, i32>(0)? as i64,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(dberr(format!("reading {table}")))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(dberr(format!("reading {table}")))
    }
}

impl PyLake {
    /// Lock the connection, recovering from a poisoned mutex (a panic in a prior
    /// pymethod while holding the lock) instead of bricking the handle into a
    /// permanent `PanicException` loop.
    fn locked_conn(&self) -> std::sync::MutexGuard<'_, Option<Connection>> {
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn query_ipc_bytes(&self, sql: &str, values: &[DuckValue]) -> anyhow::Result<Vec<u8>> {
        let guard = self.locked_conn();
        let conn = guard.as_ref().context("operation on closed BidsLake")?;
        let mut stmt = conn.prepare(sql).map_err(duck).context("preparing query")?;
        let arrow = stmt
            .query_arrow(params_from_iter(values.iter()))
            .map_err(duck)
            .context("executing query")?;
        let schema = arrow.get_schema();
        let batches: Vec<RecordBatch> = arrow.collect();

        let mut buf: Vec<u8> = Vec::new();
        {
            let mut writer =
                StreamWriter::try_new(&mut buf, schema.as_ref()).context("arrow ipc writer")?;
            for batch in &batches {
                writer.write(batch).context("writing arrow batch")?;
            }
            writer.finish().context("finishing arrow stream")?;
        }
        Ok(buf)
    }
}

/// Resolve a dataset-relative `file_path` against a `root_uri`
/// (`file:///abs/path` or `s3://bucket/prefix`) into a full URI, collapsing the
/// join to a single separator.
#[pyfunction]
fn resolve_uri(root_uri: &str, file_path: &str) -> String {
    let root = root_uri.trim_end_matches('/');
    let rel = file_path.trim_start_matches('/');
    if rel.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{rel}")
    }
}

/// Convert a Python scalar into a DuckDB bind value. `bool` is checked before
/// `int` because Python `bool` is an `int` subclass.
fn py_to_duck_value(ob: &Bound<'_, PyAny>) -> PyResult<DuckValue> {
    if ob.is_none() {
        return Ok(DuckValue::Null);
    }
    if let Ok(b) = ob.downcast::<PyBool>() {
        return Ok(DuckValue::Boolean(b.is_true()));
    }
    if let Ok(i) = ob.extract::<i64>() {
        return Ok(DuckValue::BigInt(i));
    }
    if let Ok(f) = ob.extract::<f64>() {
        return Ok(DuckValue::Double(f));
    }
    if let Ok(s) = ob.extract::<String>() {
        return Ok(DuckValue::Text(s));
    }
    Err(PyTypeError::new_err(format!(
        "unsupported SQL parameter type: {}",
        ob.get_type().name()?
    )))
}

/// An [`anyhow::Error`] as a Python exception, **with its whole context chain**.
///
/// `{:#}` rather than `{}`, and the difference is the entire point. anyhow's plain
/// `Display` prints only the outermost `.context()`, so a chain like
/// `duckdb::Error(…) -> .context("preparing query")` rendered as exactly
/// `preparing query`, discarding the message that said what was actually wrong. Every SQL
/// error a caller could make arrived identical:
///
/// ```text
/// 'select * from nope'              -> RuntimeError: preparing query
/// 'select nosuchcol from all_files' -> RuntimeError: preparing query
/// 'select * from all_files where'   -> RuntimeError: preparing query
/// ```
///
/// while DuckDB had said "Table with name nope does not exist! Did you mean …", "Binder
/// Error: Referenced column … not found" and "Parser Error: syntax error at or near"
/// respectively. `PyLake` is the only way to run a statement and `sibling()`-shaped
/// queries are wide and hand-composed, so this was the error a user met most often and
/// the one that told them least.
///
/// The alternate form walks the chain and joins it with `: `, outermost first, so the
/// caller gets both what bidslake was doing and what the engine said.
fn anyhow_err(e: anyhow::Error) -> PyErr {
    PyRuntimeError::new_err(format!("{e:#}"))
}

/// A `map_err` for a bare [`duckdb::Error`], naming what bidslake was doing.
///
/// The queries these guard are bidslake's own and fixed, so DuckDB's message is the
/// diagnostic part — but on its own it says "Catalog Error: Table with name
/// bidslake_meta does not exist" with no hint of which accessor asked for it. One
/// prefix is the whole fix, and it keeps the call sites to a single combinator.
fn dberr(what: impl Into<String>) -> impl FnOnce(duckdb::Error) -> PyErr {
    let what = what.into();
    move |e| PyRuntimeError::new_err(format!("{what}: {e}"))
}

/// `operation on closed BidsLake`, the one error that is not the engine's.
fn closed() -> PyErr {
    PyRuntimeError::new_err("operation on closed BidsLake")
}

/// A [`duckdb::Error`] as a *leaf* [`anyhow::Error`], discarding its `source()`.
///
/// `duckdb::Error`'s own `Display` is the complete engine diagnostic — the error class,
/// the offending identifier, the "Did you mean …" suggestion and the `LINE n:` caret.
/// Its source is the raw FFI code, whose `Display` is `Error code 1: Unknown error code`.
/// So walking the whole chain (which is what `{:#}` does, and what we want everywhere
/// else) appends that to every message:
///
/// ```text
/// preparing query: Catalog Error: Table with name nope does not exist! …
///     ^ useful                                                          ^
///     : Error code 1: Unknown error code   <- always, and never informative
/// ```
///
/// Re-wrapping the message as a fresh error truncates the chain exactly where it stops
/// being worth reading, and keeps `{:#}` honest for the contexts above it.
fn duck(e: duckdb::Error) -> anyhow::Error {
    anyhow::anyhow!("{e}")
}

/// A compiled, validated bidslake **layout**: the concepts-to-path direction, for
/// naming an output file before it exists.
///
/// Rendering lives here rather than in Python so there is exactly one implementation
/// of the template semantics, and because constructing it is what runs the round-trip
/// check against the layout's term map (see `bids_schema::layout`) — a layout whose
/// rendered paths do not read back as declared fails to load rather than silently
/// producing a tree nothing can index.
#[pyclass(module = "bidslake._bidslake")]
struct PyLayout {
    layout: bids_schema::layout::Layout,
    name: String,
}

#[pymethods]
impl PyLayout {
    /// Load a bundled layout by name (`feat`).
    #[new]
    fn new(name: &str) -> PyResult<Self> {
        let layout = bids_schema::layout::bundled_layout(name).ok_or_else(|| {
            PyRuntimeError::new_err(format!(
                "unknown layout {name:?}; bundled layouts are {:?}",
                bids_schema::layout::BUNDLED_LAYOUT_NAMES
            ))
        })?;
        Ok(Self {
            layout,
            name: name.to_string(),
        })
    }

    /// The declared role names, sorted.
    fn roles(&self) -> Vec<String> {
        self.layout.roles().into_iter().map(String::from).collect()
    }

    /// The term map this layout is checked against.
    fn term_map(&self) -> &str {
        self.layout.term_map_name()
    }

    /// One role's human-readable description, if it declares one.
    fn description(&self, role: &str) -> Option<String> {
        self.layout.role(role).and_then(|r| r.description.clone())
    }

    /// Render `role` relative to a unit's output root. `None` when the role is unknown
    /// or a `{placeholder}` is unbound — an unbound placeholder must not render as
    /// empty, since that yields a plausible path pointing at the wrong file.
    fn render(&self, role: &str, bindings: BTreeMap<String, String>) -> Option<String> {
        self.layout.render(role, &bindings)
    }

    /// Every role that renders under `root`, with what the filesystem says about it.
    ///
    /// Returns `(role, exists, size_bytes, mtime_ns)` per role, in the layout's own order.
    /// A role whose placeholders `bindings` does not supply is **omitted** rather than
    /// reported absent: unaddressable and missing are different answers, and a caller that
    /// conflated them would report a tree incomplete because it forgot a binding.
    ///
    /// One `stat` per role, here rather than in Python, so that "is this role present, and
    /// has it changed" has one implementation — the same question `bidslake verify` asks of
    /// a catalog's registry rows, against the same `FileStat` meaning of size and mtime.
    /// Twenty-three sequential stats is not worth overlapping; `verify`'s million are, and
    /// that path uses the concurrent `BidsFileSystem::stat_many`.
    fn present(
        &self,
        root: &str,
        bindings: BTreeMap<String, String>,
    ) -> Vec<(String, bool, Option<u64>, Option<i64>)> {
        let root = std::path::Path::new(root);
        self.layout
            .roles()
            .iter()
            .filter_map(|role| {
                let rel = self.layout.render(role, &bindings)?;
                let path = root.join(rel);
                let stat = std::fs::metadata(&path)
                    .ok()
                    .as_ref()
                    .and_then(bidslake::fs::FileStat::from_metadata);
                Some(match stat {
                    Some(st) => (
                        (*role).to_string(),
                        true,
                        Some(st.size_bytes),
                        Some(st.mtime_ns),
                    ),
                    // `exists` is false for anything that could not be stat-ed, which
                    // folds "absent" together with "there but unreadable". For a tree this
                    // process just wrote, that distinction has no consumer.
                    None => ((*role).to_string(), false, None, None),
                })
            })
            .collect()
    }

    fn __repr__(&self) -> String {
        format!(
            "PyLayout({:?}, roles={})",
            self.name,
            self.layout.roles().len()
        )
    }
}

#[pymodule]
fn _bidslake(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyLake>()?;
    m.add_class::<PyLayout>()?;
    m.add_function(wrap_pyfunction!(resolve_uri, m)?)?;
    Ok(())
}
