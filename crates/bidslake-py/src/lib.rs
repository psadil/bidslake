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
        let config = Config::default().access_mode(mode).map_err(anyhow_err)?;
        let conn = Connection::open_with_flags(path, config)
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
        let conn = guard
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("operation on closed BidsLake"))?;
        let mut stmt = conn
            .prepare(
                "SELECT table_name FROM information_schema.tables \
                 WHERE table_schema = 'main' ORDER BY table_name",
            )
            .map_err(anyhow_err_from)?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(anyhow_err_from)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(anyhow_err_from)
    }

    /// The `(column_name, data_type)` pairs of `table`, in ordinal order.
    /// Includes the generated virtual columns (they are real catalog columns).
    fn columns(&self, table: &str) -> PyResult<Vec<(String, String)>> {
        let guard = self.locked_conn();
        let conn = guard
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("operation on closed BidsLake"))?;
        let mut stmt = conn
            .prepare(
                "SELECT column_name, data_type FROM information_schema.columns \
                 WHERE table_schema = 'main' AND table_name = ? \
                 ORDER BY ordinal_position",
            )
            .map_err(anyhow_err_from)?;
        let rows = stmt
            .query_map([table], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(anyhow_err_from)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(anyhow_err_from)
    }

    /// The `bidslake_meta` version stamp `(schema_version, bids_version,
    /// bidslake_version)`, or `None` if the DB predates the stamp.
    fn meta(&self) -> PyResult<Option<(String, String, String)>> {
        let guard = self.locked_conn();
        let conn = guard
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("operation on closed BidsLake"))?;
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
            Err(e) => Err(anyhow_err_from(e)),
        }
    }

    /// The full effective (base + overlays) schema JSON stamped into a database built
    /// with schema overlays, or `None` if the DB carries no augmentation (it was
    /// indexed without overlays, or predates the stamp). The stubgen reads this to
    /// generate static types for augmented entities/suffixes/columns.
    fn effective_schema(&self) -> PyResult<Option<String>> {
        let guard = self.locked_conn();
        let conn = guard
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("operation on closed BidsLake"))?;
        let mut stmt =
            match conn.prepare("SELECT effective_schema::VARCHAR FROM bidslake_schema LIMIT 1") {
                Ok(stmt) => stmt,
                // Table absent (un-augmented or older DB) → no stored effective schema.
                Err(_) => return Ok(None),
            };
        match stmt.query_row([], |r| r.get::<_, String>(0)) {
            Ok(v) => Ok(Some(v)),
            Err(duckdb::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(anyhow_err_from(e)),
        }
    }

    /// The applied overlays' provenance `(idx, source, sha256)` in application order,
    /// or an empty list if the DB carries no augmentation.
    fn overlays(&self) -> PyResult<Vec<(i64, String, String)>> {
        let guard = self.locked_conn();
        let conn = guard
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("operation on closed BidsLake"))?;
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
            .map_err(anyhow_err_from)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(anyhow_err_from)
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
        let conn = guard
            .as_ref()
            .ok_or_else(|| PyRuntimeError::new_err("operation on closed BidsLake"))?;
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
            .map_err(anyhow_err_from)?;
        rows.collect::<Result<Vec<_>, _>>().map_err(anyhow_err_from)
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
        let mut stmt = conn.prepare(sql).context("preparing query")?;
        let arrow = stmt
            .query_arrow(params_from_iter(values.iter()))
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

fn anyhow_err(e: impl std::fmt::Display) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

fn anyhow_err_from(e: duckdb::Error) -> PyErr {
    PyRuntimeError::new_err(e.to_string())
}

#[pymodule]
fn _bidslake(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyLake>()?;
    m.add_function(wrap_pyfunction!(resolve_uri, m)?)?;
    Ok(())
}
