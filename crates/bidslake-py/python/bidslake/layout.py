"""The BIDSLayout-analog: open a bidslake DuckDB database and query it."""

from __future__ import annotations

import json
import os
import warnings
from collections.abc import Iterator, Mapping, Sequence
from string.templatelib import Interpolation, Template
from typing import Any, Unpack

# Import the types by name: the `Table.pl`/`Table.lazy` methods would otherwise
# shadow a `pl` module alias inside the class body's annotations.
from polars import DataFrame, LazyFrame
from sqlalchemy.sql import ClauseElement
from upath import UPath

from . import _bidslake
from ._arrow import ipc_to_df
from ._lazy import build_lazy
from ._sql import ALL_FILES, compile_statement, quote_ident
from .file import BidsFile
from .paths import to_upath, to_uri
from .relations import Relation
from .schema._generated import SCHEMA_VERSION, GetFilters


class Table:
    """A queryable view of one database table, materializable as Polars/Arrow.

    The view may instead be over a derived SQL query, as the wide `files` view is.
    """

    def __init__(self, lake: BidsLake, name: str, *, sql: str | None = None) -> None:
        """Bind a table name to the catalog that will run the query.

        Args:
            lake: The opened catalog the rows are read from.
            name: The table or view name, which is also what `repr` shows.
            sql: A `SELECT` to read instead of the named table, for a derived view.
        """
        self._lake = lake
        self._name = name
        self._sql = sql

    def _base_sql(self) -> str:
        return self._sql if self._sql is not None else f"SELECT * FROM {quote_ident(self._name)}"

    def pl(self) -> DataFrame:
        """The whole table as an eager Polars DataFrame (virtual columns included)."""
        return self._lake._query(self._base_sql(), [])

    def lazy(self) -> LazyFrame:
        """A Polars LazyFrame over the table.

        Backed by a Polars IO source that pushes column projection into DuckDB and applies
        predicates via Polars (see `_lazy`). Projection pushdown is the win for wide tables.
        """
        return build_lazy(self._lake, self._base_sql())

    def arrow(self) -> Any:
        """The table as a `pyarrow.Table` (requires pyarrow)."""
        return self.pl().to_arrow()

    def __repr__(self) -> str:
        """The table's name, as `Table('scans')`."""
        return f"Table({self._name!r})"


class BidsLake:
    """An opened bidslake database.

    Exposes each table as a `Table` and the headline `get` iterator that yields `BidsFile`
    handles for files matching BIDS-concept filters.
    """

    def __init__(
        self,
        path: str,
        *,
        read_only: bool = True,
        base_dir: str | os.PathLike[str] | None = None,
        root_override: Mapping[str, str | os.PathLike[str]] | None = None,
    ) -> None:
        """Open the catalog at `path`.

        Args:
            path: The DuckDB database to open.
            read_only: Open without taking DuckDB's write lock, which is the default; a
                writable handle holds that lock until `close`.
            base_dir: Rebase every root under this parent, keeping the root's own directory
                name.
            root_override: Redirect one root, keyed by its `root_uri`, or a whole dataset,
                keyed by `dataset_id`. A per-root entry wins over a per-dataset one, and
                either wins over `base_dir`.
        """
        self._lake = _bidslake.PyLake(str(path), read_only)
        self._path = str(path)
        self._col_cache: dict[str, dict[str, str]] = {}
        self._root_uris: dict[str, list[str]] | None = None
        # Path rebasing: a stored `root_uri` is absolute to the ingest host, so a
        # moved dataset (or another machine) needs it redirected at query time.
        self._base_dir = to_uri(base_dir) if base_dir is not None else None
        self._root_override = {k: to_uri(v) for k, v in (root_override or {}).items()}
        self._warn_on_version_mismatch()

    # -- lifecycle ---------------------------------------------------------

    def close(self) -> None:
        """Close the underlying DuckDB connection.

        Releases its file handle and — for a `read_only=False` handle — its write lock,
        without waiting for garbage collection. Idempotent; any later query raises
        `RuntimeError`.
        """
        self._lake.close()

    def __enter__(self) -> BidsLake:
        """The lake itself, so `with BidsLake(path) as lake` binds the open handle."""
        return self

    def __exit__(self, *exc: object) -> None:
        """Close on the way out, whether or not the block raised."""
        self.close()

    # -- table access ------------------------------------------------------

    @property
    def all_files(self) -> Table:
        """One row per file the walk saw — every kind, with its BIDS concepts.

        The dataset's manifest: data files, sidecars, tabular files, gradients, and the
        documentation a dataset ships. Every file-keyed table joins this on `file_id`.
        """
        return Table(self, ALL_FILES)

    @property
    def scans(self) -> Table:
        """The BIDS `scans.tsv` table: acquisition metadata per data file.

        Not the file registry — that is `all_files`. Keyed by `file_id`, so join it to
        `all_files` to see which file a row is about.
        """
        return Table(self, "scans")

    @property
    def sidecars(self) -> Table:
        """The JSON sidecar fields of each data file, with inheritance already resolved.

        Keyed by `file_id`: one row per described file, holding the nearest-wins merge of
        every sidecar that applies to it, so an inherited `task-*_bold.json` costs no second
        lookup. Schema fields are columns under their BIDS names; anything else is JSON in
        `other_data`. `BidsFile.metadata` is the same content for a single file.
        """
        return Table(self, "sidecars")

    @property
    def participants(self) -> Table:
        """The BIDS `participants.tsv` table: one row per person per dataset.

        Keyed by `(dataset_id, participant_id)` rather than by file, so a dataset spread over
        several ingest roots (docs/adr/0005) still lists each person once. `participant_id`
        carries the `sub-` prefix, as it does in the TSV. Declared columns (`age`, `sex`, …)
        are typed; anything else is JSON in `other_data`.
        """
        return Table(self, "participants")

    @property
    def sessions(self) -> Table:
        """The BIDS `sessions.tsv` table: one row per session of one participant.

        Keyed by `(dataset_id, participant_id, session_id)` — entities the file states
        outright, not a `file_id` (docs/adr/0003). Declared columns (`acq_time`, …) are
        typed; anything else is JSON in `other_data`.
        """
        return Table(self, "sessions")

    @property
    def events(self) -> Table:
        """Every task-event row in the catalog, as it is stored.

        Keyed by the `file_id` of the `events.tsv` itself, not of the run it describes: an
        inherited events file is stored once and pointed at each of its runs through
        `file_associations` (docs/adr/0003). `BidsFile.get_events` is that join for one data
        file, ordered by `onset`.
        """
        return Table(self, "events")

    @property
    def files(self) -> Table:
        """One row per data file, widened with sidecar/participant/dataset columns.

        Hides the joins (sidecars and scans by `file_id`; files↔participants by
        `sub`/path-prefix). Joined-table columns are namespaced
        `sidecar__*`/`participant__*`/`dataset__*`/`scan__*` (BIDS's own `__` convention) so
        they never collide with the registry's own columns.
        """
        return Table(self, "files", sql=self._files_sql())

    def table(self, name: str) -> Table:
        """A `Table` for any table in the database (validated).

        Raises:
            KeyError: If the database holds no table of that name; the message lists the
                ones it does.
        """
        if name not in self.tables():
            raise KeyError(f"no table {name!r}; available: {sorted(self.tables())}")
        return Table(self, name)

    def tables(self) -> list[str]:
        """Every base table and view in the database."""
        return self._lake.list_tables()

    def resolve(self, dataset_id: str, file_path: str, root_uri: str | None = None) -> UPath:
        r"""One handle that opens the file a registry row names.

        The same resolution `BidsFile.path` uses — honoring `base_dir` and `root_override` —
        but for any row in any table, not just `all_files`.

        Its main use is reading what the catalog deliberately did not store. A table whose
        ingestion policy is `undeclared: catalog` keeps only the columns its schema declares;
        the file on disk stays the record of the rest, and the file registry is the index of
        those files:

            row = (lake.all_files.pl()
                   .filter(pl.col("file_path").str.contains("confounds.tsv"))
                   .row(0, named=True))
            path = lake.resolve(row["dataset_id"], row["file_path"], row["root_uri"])
            full = pl.read_csv(path.open("rb"), separator="\t", null_values="n/a")
            full.select("^a_comp_cor_.*$")   # the columns the catalog did not store

        To learn which names those are without touching disk, read
        `tabular_undeclared_columns`.

        Args:
            dataset_id: The dataset the row belongs to.
            file_path: The row's path, relative to the root it was walked from.
            root_uri: The root the file was walked from. Optional only because a single-root
                dataset has nothing to disambiguate; pass it (every file-keyed row can reach
                it through `file_registry`) for a dataset that spans several roots, which is
                what a study processed one subject at a time falls into (docs/adr/0005).

        Returns:
            A `upath.UPath`. Read through its `.open()` rather than `str(path)`: a `UPath`
            stringifies back to a URI, scheme and all, and going through it is what keeps
            the recipe above working for a remote dataset.
        """
        return to_upath(self._resolve(dataset_id, root_uri, file_path))

    # -- cross-dataset links (docs/adr/0003) -------------------------------

    def datasets(self) -> DataFrame:
        """One row per dataset in the catalog (the `dataset_description` table)."""
        return self._query("SELECT * FROM dataset_description", [])

    def roots(self, dataset_id: str | None = None) -> DataFrame:
        """The ingest roots in this catalog: `(dataset_id, root_uri, tenure)`.

        A dataset may have several (`docs/adr/0005`), so `root_uri` is the only thing that
        says *which tree* a row is about — and `file_path` is meaningless without it.

        `tenure` says what may be concluded from those rows (`docs/adr/0007`). `attached`
        means somebody else writes there and the catalog records what it saw last time it
        looked; `managed` means bidslake owns the storage, so its rows are current by
        construction. The distinction matters most to anything deciding whether work still
        needs doing: a catalog is sound for *finding* work in either tier, and for *skipping*
        it only once an attached root's rows have been confirmed — by `bidslake verify`, or
        by checking the files.

        Pair with `bidslake.to_uri` to scope a query to one tree:

            lake.sql("SELECT ... WHERE root_uri = ?", [bidslake.to_uri(out_dir)])

        A catalog written before `tenure` existed has no such column, and this reads
        `attached` for it rather than failing. That is not leniency for its own sake: the
        write side backfills the same value (`ALTER TABLE … DEFAULT 'attached'`), but only
        `index` runs DDL, and this package opens read-only — so a reader that insisted on the
        column would simply be unable to open an older catalog at all.

        Args:
            dataset_id: One dataset's roots; `None` for every dataset's.
        """
        # Selected as a literal when absent, so the shape of the frame does not depend on how
        # old the catalog is; a caller can always read `tenure`.
        tenure = "tenure" if self._has_column("dataset_roots", "tenure") else "'attached' AS tenure"
        sql = f"SELECT dataset_id, root_uri, {tenure} FROM dataset_roots"
        if dataset_id is None:
            return self._query(f"{sql} ORDER BY dataset_id, root_uri", [])
        return self._query(f"{sql} WHERE dataset_id = ? ORDER BY root_uri", [dataset_id])

    def _has_column(self, table: str, column: str) -> bool:
        """Does `table` carry `column` in *this* catalog?

        Catalogs outlive the schema that built them, and a column added to an existing table
        only reaches an old file through `index`'s DDL. Read paths therefore cannot assume
        the current shape — asking is cheaper than the alternative, which is a `Binder Error`
        surfaced to someone whose only mistake was not re-indexing.
        """
        df = self._query(
            "SELECT 1 FROM duckdb_columns() WHERE table_name = ? AND column_name = ? LIMIT 1",
            [table, column],
        )
        return df.height > 0

    def dataset_relations(self) -> DataFrame:
        """The resolved dataset-to-dataset relations.

        Columns `(from_dataset_id, to_dataset_id, relation, via_identity)`, where `relation`
        is one of `Relation`. Resolved at query time from each dataset's declared
        `SourceDatasets` — order of ingest does not matter.
        """
        self._require_relations()
        return self._query("SELECT * FROM dataset_relations", [])

    def related_datasets(
        self, dataset_id: str, relation: Relation | str | None = None
    ) -> list[str]:
        """The dataset ids related to `dataset_id` by explicit provenance.

        A shared-source link guarantees a shared subject/entity namespace, so a caller can
        then *soundly* match files across the boundary — bidslake resolves the dataset
        relation; the caller does the entity match:

            for other in lake.related_datasets(fp_id, relation=Relation.SHARES_SOURCE):
                lake.get(dataset_id=other, sub=f.sub, ses=f.ses, task=f.task, run=f.run)

        Args:
            dataset_id: The dataset the links start from.
            relation: One kind of link to filter to, e.g. `Relation.SHARES_SOURCE`; `None`
                returns every related dataset.
        """
        self._require_relations()
        sql = "SELECT DISTINCT to_dataset_id FROM dataset_relations WHERE from_dataset_id = ?"
        params: list[Any] = [dataset_id]
        if relation is not None:
            sql += " AND relation = ?"
            params.append(str(relation))
        sql += " ORDER BY to_dataset_id"
        df = self._query(sql, params)
        return list(df["to_dataset_id"]) if df.height else []

    def _require_relations(self) -> None:
        if "dataset_relations" not in self.tables():
            raise RuntimeError(
                "this catalog predates cross-dataset links; run "
                "`bidslake link init <db>` or re-index to add them"
            )

    # -- schema augmentation -----------------------------------------------

    @property
    def overlays(self) -> list[tuple[int, str, str]]:
        """The schema overlays applied when this database was indexed.

        `(index, source, sha256)` in application order — empty if none. Augmented columns and
        tables are queryable with no extra step (`get` and the table accessors validate
        against the live database), so this is for provenance/introspection. For *static*
        typing of augmented columns, generate a project-local module with
        `python -m bidslake.stubgen`.
        """
        return self._lake.overlays()

    @property
    def term_maps(self) -> list[tuple[int, str, str]]:
        """The BEP-043 term maps applied when this database was indexed.

        `(index, source, sha256)` in application order — empty if none. An adapter
        (`--adapter freesurfer`) projects a standardized *non-BIDS* dataset onto BIDS
        concepts via a term map, declares its tables via a BIDS overlay (see `overlays`), and
        its read/catalog policy via an ingestion schema (see `ingestion`). The resulting
        tables are queryable with no extra step (`lake.table("freesurfer_aparc")`); this is
        for provenance/introspection.
        """
        return self._lake.term_maps()

    @property
    def ingestion(self) -> list[tuple[int, str, str]]:
        """The ingestion schemas applied when this database was indexed.

        `(index, source, sha256)` in application order — empty if none.
        """
        return self._lake.ingestion()

    def effective_schema(self) -> dict[str, Any] | None:
        """The full effective (base + overlays) BIDS schema stamped into the database.

        Every database embeds its schema, so this recovers exactly what the catalog was built
        from.

        Returns:
            The schema as parsed JSON, or `None` for a database that predates the stamp.
        """
        raw = self._lake.effective_schema()
        return json.loads(raw) if raw is not None else None

    # -- the headline iterator --------------------------------------------

    def get(
        self,
        *,
        table: str = ALL_FILES,
        **filters: Unpack[GetFilters],
    ) -> Iterator[BidsFile]:
        """Yield `BidsFile` for rows of `table` matching `filters`.

        **Every file the walk saw**, not only the imaging ones: a `*_bold.nii.gz` and the
        `*_bold.json` beside it are both rows here. Narrow with `kind="data"` when you mean
        the primary data files, or with `extension` when you mean one format. There is one
        registry, and which slice of it you want is yours to say:

            lake.get(task="rest", suffix="bold", kind="data")
            lake.get(task="rest", suffix="bold", extension=".nii.gz")

        Args:
            table: The table to iterate; the whole file registry by default.
            **filters: One keyword per column (BIDS entity, `datatype`/`suffix`/`extension`/
                `modality`, or `dataset_id`). A scalar matches by equality, a sequence by
                `IN (...)`, and `None` by `IS NULL` — so `ses=None` selects sessionless
                files. With no filters, iterates the whole table across every dataset in the
                database.

        Yields:
            One `BidsFile` per matching row, carrying the row's BIDS concepts and a path
            already resolved through `base_dir`/`root_override`.

        Note:
            The result set is materialized in full (the Arrow-IPC buffer is read into a
            Polars frame) before any row is yielded, so peak memory is the whole result set —
            the generator form is for ergonomics, not streaming. Genuine streaming awaits the
            PyCapsule bridge (see `src/lib.rs`).
        """
        where, params = self._compile_filters(table, filters)
        sql = f"SELECT * FROM {self._relation(table)}"
        if where:
            sql += f" WHERE {where}"
        df = self._query(sql, params)
        for row in df.iter_rows(named=True):
            yield BidsFile._from_row(
                row["dataset_id"],
                row["root_uri"],
                row["file_path"],
                row["file_id"],
                self._resolve(row["dataset_id"], row["root_uri"], row["file_path"]),
                row,
                self,
            )

    # -- escape hatch ------------------------------------------------------

    def sql(
        self, query: str | Template | ClauseElement, params: Sequence[Any] | None = None
    ) -> DataFrame:
        """Run a query and return the result as Polars.

        The two non-string forms are lowered here and executed by the same Rust engine as a
        plain string, so a query built either way opens no second connection and comes back
        over the same Arrow bridge:

            lake.sql(t"SELECT * FROM all_files WHERE suffix = {suffix}")

            from sqlalchemy import select
            from bidslake.schema.models import AllFiles, Sidecars
            lake.sql(
                select(AllFiles.file_path, Sidecars.RepetitionTime)
                .join(Sidecars, Sidecars.file_id == AllFiles.file_id)
                .where(AllFiles.task == "rest", AllFiles.kind == "data")
            )

        That the statement is *compiled* here rather than executed is the whole reason
        SQLAlchemy is a dependency: it is the query builder `get` deliberately is not —
        joins, `OR`, comparisons, subqueries — with none of its session or engine machinery.

        Args:
            query: A plain SQL string; a PEP 750 t-string, whose interpolations are lowered
                to DuckDB bind parameters — never string-concatenated — so values can't
                inject SQL; or a SQLAlchemy statement.
            params: Positional bind parameters, for the plain-string form.

        Returns:
            The whole result set as a Polars DataFrame.
        """
        if isinstance(query, ClauseElement):
            return self._query(*compile_statement(query))
        if isinstance(query, Template):
            text_parts: list[str] = []
            values: list[Any] = []
            for item in query:
                if isinstance(item, Interpolation):
                    text_parts.append("?")
                    values.append(item.value)
                else:
                    text_parts.append(item)
            return self._query("".join(text_parts), values)
        return self._query(query, list(params) if params else [])

    def columns(self, table: str) -> dict[str, str]:
        """The `{column_name: duckdb_type}` mapping of `table`."""
        return dict(self._columns(table))

    # -- internals ---------------------------------------------------------

    def _query(self, sql: str, params: list[Any]) -> DataFrame:
        return ipc_to_df(self._lake.query_ipc(sql, params))

    def _columns(self, table: str) -> dict[str, str]:
        cached = self._col_cache.get(table)
        if cached is None:
            # A synthetic relation has no columns of its own to introspect — it has
            # exactly the view's.
            cols = self._lake.columns(table)
            if not cols:
                raise KeyError(f"no table {table!r}; available: {sorted(self.tables())}")
            cached = dict(cols)
            self._col_cache[table] = cached
        return cached

    def _relation(self, table: str) -> str:
        """`table` as a FROM item, widened with the file registry where it needs to be.

        Two shapes. A file-keyed satellite (`scans`, `events`, `sidecars`, an adapter's
        tables) holds a `file_id` and its own measurements — the BIDS concepts live once,
        on the registry, instead of being copied onto all two dozen of them
        (docs/adr/0006) — so it is joined back to them. Everything else, including the
        registry itself and the entity-keyed tables, stands alone.
        """
        own = self._columns(table)
        if table == ALL_FILES or "file_id" not in own:
            return quote_ident(table)
        borrowed = ", ".join(
            f"_f.{quote_ident(c)}" for c in self._columns(ALL_FILES) if c not in own
        )
        return (
            f"(SELECT _t.*, {borrowed} FROM {quote_ident(table)} _t "
            f"JOIN {ALL_FILES} _f USING (file_id))"
        )

    def _filter_columns(self, table: str) -> dict[str, str]:
        """Every column a query against `table` may name.

        Its own, plus the registry's for a file-keyed one, which `_relation` reaches by
        joining `file_id`. Kept apart from `columns`, which stays a faithful report of what
        the database holds.
        """
        own = self._columns(table)
        if table == ALL_FILES or "file_id" not in own:
            return own
        return {**self._columns(ALL_FILES), **own}

    def _compile_filters(self, table: str, filters: Mapping[str, Any]) -> tuple[str, list[Any]]:
        cols = self._filter_columns(table)
        clauses: list[str] = []
        params: list[Any] = []
        for key, val in filters.items():
            if key not in cols:
                raise KeyError(f"column {key!r} not in table {table!r}; available: {sorted(cols)}")
            ident = quote_ident(key)
            if val is None:
                clauses.append(f"{ident} IS NULL")
            elif isinstance(val, (list, tuple, set, frozenset)):
                vals = list(val)
                if not vals:
                    clauses.append("FALSE")  # `IN ()` matches nothing
                else:
                    placeholders = ", ".join("?" * len(vals))
                    clauses.append(f"{ident} IN ({placeholders})")
                    params.extend(vals)
            else:
                clauses.append(f"{ident} = ?")
                params.append(val)
        return " AND ".join(clauses), params

    def _resolve(self, dataset_id: str, root_uri: str | None, file_path: str) -> str:
        root = self._effective_root(dataset_id, root_uri)
        if root is None:
            return file_path
        return _bidslake.resolve_uri(root, file_path)

    def _effective_root(self, dataset_id: str, root_uri: str | None) -> str | None:
        """The root URI to resolve a file against, honoring `root_override` and `base_dir`.

        `base_dir` rebases under a new parent, keeping the directory name.

        Args:
            dataset_id: The dataset the file belongs to.
            root_uri: The file's own root — a dataset may have several (docs/adr/0005), and
                which one a file belongs to is a property of the file, so it travels with
                the row rather than being looked up per dataset. `None` for a caller that
                has only a dataset, which then resolves only if that dataset has exactly
                one root.
        """
        if root_uri is None:
            roots = self._original_roots().get(dataset_id, [])
            if len(roots) > 1:
                raise ValueError(
                    f"dataset {dataset_id!r} was built from {len(roots)} ingest roots "
                    f"({', '.join(sorted(roots))}); resolving a file needs the root it came "
                    f"from. Pass root_uri=, or root_override={{{dataset_id!r}: <uri>}}."
                )
            root_uri = roots[0] if roots else None
        # A per-root override wins over a per-dataset one, which wins over the stored root.
        if root_uri is not None and root_uri in self._root_override:
            return self._root_override[root_uri]
        if dataset_id in self._root_override:
            return self._root_override[dataset_id]
        if self._base_dir is not None and root_uri is not None:
            name = root_uri.rstrip("/").rsplit("/", 1)[-1]
            return f"{self._base_dir}/{name}"
        return root_uri

    def _original_roots(self) -> dict[str, list[str]]:
        """Every dataset's ingest roots, from `dataset_roots` (docs/adr/0005).

        Read from that table rather than `dataset_description`, which no longer carries a
        `root_uri`: a dataset can have several roots, and one column cannot hold them.
        """
        if self._root_uris is None:
            df = self._query("SELECT dataset_id, root_uri FROM dataset_roots", [])
            roots: dict[str, list[str]] = {}
            for dataset_id, root_uri in zip(df["dataset_id"], df["root_uri"], strict=True):
                roots.setdefault(dataset_id, []).append(root_uri)
            self._root_uris = roots
        return self._root_uris

    # -- BidsFile lazy lookups --------------------------------------------

    def _sidecar_metadata(self, file_id: int) -> dict[str, Any]:
        df = self._query("SELECT * FROM sidecars WHERE file_id = ?", [file_id])
        if df.height == 0:
            return {}
        row = df.row(0, named=True)
        # `other_data` holds custom (non-schema) fields in original BIDS case; the
        # typed columns hold the schema fields (also BIDS-cased). Merge both.
        # It is absent or NULL when the sidecar's ingestion policy is
        # `undeclared: catalog` (fMRIPrep's confounds sidecars, say), in which case the
        # merged metadata is the declared fields only and the file on disk holds the
        # rest — `lake.resolve()` opens it.
        meta: dict[str, Any] = {}
        other = row.get("other_data")
        if other:
            meta.update(json.loads(other))
        # `file_id` is the join key, not a metadata field (docs/adr/0006): skipping it is what
        # keeps the key itself out of the returned mapping, beside `RepetitionTime`.
        for key, value in row.items():
            if key in ("file_id", "other_data"):
                continue
            if value is not None:
                meta[key] = value
        return meta

    def _rows_for(
        self,
        file_id: int,
        association_type: str,
        table: str | None = None,
        order_by: str | None = None,
    ) -> DataFrame:
        """Rows of the file that *describes* `file_id`, reached through `file_associations`.

        The one shape every such relation takes (docs/adr/0003): the rows are stored once,
        keyed by the describing file, and the edge says which data files they are about. An
        inherited describing file — ds114's root `task-*_events.tsv` over 20 BOLD runs, or
        its `dwi.bval` over 20 images — is one stored copy and N edges, so this is a lookup
        rather than a scan over duplicated rows.

        Args:
            file_id: The described file, whose edges are followed.
            association_type: The edge kind, which names the table by default: a
                schema-declared edge is named for the table it feeds (`events`), and an
                overlay-declared one is namespaced to it (`fmriprep_confounds`), so the map
                is the identity in both cases.
            table: The table to read the rows from, when it is not the one
                `association_type` names.
            order_by: A column of that table to order the rows by.
        """
        tbl = table or association_type
        if tbl not in self.tables():
            # An adapter's table is absent from a catalog built without that adapter, which
            # is a legitimate "no rows" rather than an error.
            return DataFrame()
        sql = (
            f"SELECT t.* FROM file_associations fa "
            f"JOIN {quote_ident(tbl)} t ON t.file_id = fa.target_file_id "
            f"WHERE fa.source_file_id = ? AND fa.association_type = ?"
        )
        if order_by is not None:
            sql += f" ORDER BY t.{quote_ident(order_by)}"
        return self._query(sql, [file_id, association_type])

    def _events_for(self, file_id: int) -> DataFrame:
        # Ordered by `onset`, which is what addresses an event. `events` is declared
        # order-insensitive so its files are read concurrently and it has no `row_idx`;
        # `onset` is the canonical order, and BIDS asks for events.tsv to be written in it.
        return self._rows_for(file_id, "events", order_by="onset")

    def _associated_for(
        self, dataset_id: str, root_uri: str, file_id: int, kind: str | None
    ) -> list[BidsFile]:
        # `target_file_id` is NULL for a reference to a file this dataset does not ship —
        # a dangling `IntendedFor`, kept deliberately. Those still come back, as a path
        # with no id, because "the sidecar points at something missing" is the answer.
        sql = (
            "SELECT target_file_id, target_file_path, association_type "
            "FROM file_associations WHERE source_file_id = ?"
        )
        params: list[Any] = [file_id]
        if kind is not None:
            sql += " AND association_type = ?"
            params.append(kind)
        df = self._query(sql, params)
        out: list[BidsFile] = []
        for row in df.iter_rows(named=True):
            target = row["target_file_path"]
            # The target shares this file's root: an association is resolved within one
            # ingest root, so the source's root is the target's.
            out.append(
                BidsFile(
                    dataset_id=dataset_id,
                    root_uri=root_uri,
                    file_path=target,
                    file_id=row["target_file_id"],
                    uri=self._resolve(dataset_id, root_uri, target),
                    # Entities aren't re-parsed here (the target may not be a data file,
                    # e.g. an events.tsv); callers get the path + association kind.
                    entities={"association_type": row["association_type"]},
                    lake=self,
                )
            )
        return out

    def _files_sql(self) -> str:
        """Build the wide `files` SELECT, namespacing joined columns with `<table>__`."""

        def namespaced(table: str, alias: str, prefix: str, exclude: set[str]) -> str:
            cols = [c for c in self._columns(table) if c not in exclude]
            return ", ".join(f"{alias}.{quote_ident(c)} AS {quote_ident(prefix + c)}" for c in cols)

        # `file_id` is excluded from every joined table: it is the join key, so repeating it
        # under four names would say the same thing four times.
        sidecar_sel = namespaced("sidecars", "sc", "sidecar__", {"file_id"})
        scan_sel = namespaced("scans", "sn", "scan__", {"file_id"})
        participant_sel = namespaced("participants", "p", "participant__", {"dataset_id"})
        dataset_sel = namespaced("dataset_description", "dd", "dataset__", {"dataset_id"})
        parts = ["s.*", scan_sel, sidecar_sel, participant_sel, dataset_sel]
        select = ", ".join(p for p in parts if p)
        return (
            f"SELECT {select} FROM (SELECT * FROM {ALL_FILES} WHERE kind = 'data') s "
            "LEFT JOIN scans sn ON sn.file_id = s.file_id "
            "LEFT JOIN sidecars sc ON sc.file_id = s.file_id "
            "LEFT JOIN dataset_description dd ON dd.dataset_id = s.dataset_id "
            "LEFT JOIN participants p ON p.dataset_id = s.dataset_id "
            "AND ('sub-' || s.sub = p.participant_id OR s.file_path LIKE p.participant_id || '/%')"
        )

    def _warn_on_version_mismatch(self) -> None:
        meta = self._lake.meta()
        if meta is None:
            return
        schema_version, _bids_version, _bidslake_version = meta
        if schema_version != SCHEMA_VERSION:
            # Overlays add columns/tables beyond the base types this build ships; the
            # runtime introspection covers them, but static typing wants a regen.
            augmented = (
                " (augmented; run `python -m bidslake.stubgen` for static types)"
                if self._lake.overlays()
                else ""
            )
            warnings.warn(
                f"database indexed with BIDS schema {schema_version}; bidslake is "
                f"typed against {SCHEMA_VERSION}. Column names/types are validated "
                f"at runtime{augmented}.",
                stacklevel=3,
            )

    def __repr__(self) -> str:
        """The database it was opened from, as `BidsLake('study.duckdb')`."""
        return f"BidsLake({self._path!r})"
