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

from . import _bidslake
from ._arrow import ipc_to_df
from ._lazy import build_lazy
from ._sql import quote_ident
from .file import BidsFile
from .paths import to_uri
from .relations import Relation
from .schema._generated import SCHEMA_VERSION, GetFilters


class Table:
    """A queryable view of one database table (or a derived SQL query, like the
    wide ``files`` view), materializable as Polars/Arrow."""

    def __init__(self, lake: BidsLake, name: str, *, sql: str | None = None) -> None:
        self._lake = lake
        self._name = name
        self._sql = sql

    def _base_sql(self) -> str:
        return self._sql if self._sql is not None else f"SELECT * FROM {quote_ident(self._name)}"

    def pl(self) -> DataFrame:
        """The whole table as an eager Polars DataFrame (virtual columns included)."""
        return self._lake._query(self._base_sql(), [])

    def lazy(self) -> LazyFrame:
        """A Polars LazyFrame over the table, backed by a Polars IO source that
        pushes column projection into DuckDB and applies predicates via Polars
        (see ``_lazy``). Projection pushdown is the win for wide tables."""
        return build_lazy(self._lake, self._base_sql())

    def arrow(self) -> Any:
        """The table as a ``pyarrow.Table`` (requires pyarrow)."""
        return self.pl().to_arrow()

    def __repr__(self) -> str:
        return f"Table({self._name!r})"


class BidsLake:
    """An opened bidslake database.

    Exposes each table as a :class:`Table` and the headline :meth:`get` iterator
    that yields :class:`BidsFile` handles for files matching BIDS-concept filters.
    """

    def __init__(
        self,
        path: str,
        *,
        read_only: bool = True,
        base_dir: str | os.PathLike[str] | None = None,
        root_override: Mapping[str, str | os.PathLike[str]] | None = None,
    ) -> None:
        self._lake = _bidslake.PyLake(str(path), read_only)
        self._path = str(path)
        self._col_cache: dict[str, dict[str, str]] = {}
        self._root_uris: dict[str, str] | None = None
        # Path rebasing: a stored `root_uri` is absolute to the ingest host, so a
        # moved dataset (or another machine) needs it redirected at query time.
        self._base_dir = to_uri(base_dir) if base_dir is not None else None
        self._root_override = {k: to_uri(v) for k, v in (root_override or {}).items()}
        self._warn_on_version_mismatch()

    # -- lifecycle ---------------------------------------------------------

    def close(self) -> None:
        """Close the underlying DuckDB connection, releasing its file handle and
        (for a ``read_only=False`` handle) its write lock, without waiting for
        garbage collection. Idempotent; any later query raises ``RuntimeError``."""
        self._lake.close()

    def __enter__(self) -> BidsLake:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()

    # -- table access ------------------------------------------------------

    @property
    def scans(self) -> Table:
        return Table(self, "scans")

    @property
    def sidecars(self) -> Table:
        return Table(self, "sidecars")

    @property
    def participants(self) -> Table:
        return Table(self, "participants")

    @property
    def sessions(self) -> Table:
        return Table(self, "sessions")

    @property
    def events(self) -> Table:
        return Table(self, "events")

    @property
    def files(self) -> Table:
        """One row per scan, widened with sidecar/participant/dataset columns.

        Hides the two non-obvious joins (sidecars↔scans on the composite key;
        scans↔participants by ``sub``/path-prefix). Joined-table columns are
        namespaced ``sidecar__*``/``participant__*``/``dataset__*`` (BIDS's own
        ``__`` convention) so they never collide with the scans columns.
        """
        return Table(self, "files", sql=self._files_sql())

    def table(self, name: str) -> Table:
        """A :class:`Table` for any table in the database (validated)."""
        if name not in self.tables():
            raise KeyError(f"no table {name!r}; available: {sorted(self.tables())}")
        return Table(self, name)

    def tables(self) -> list[str]:
        """Every base table and view in the database."""
        return self._lake.list_tables()

    # -- cross-dataset links (docs/adr/0003) -------------------------------

    def datasets(self) -> DataFrame:
        """One row per dataset in the catalog (the ``dataset_description`` table)."""
        return self._query("SELECT * FROM dataset_description", [])

    def dataset_relations(self) -> DataFrame:
        """The resolved dataset-to-dataset relations.

        Columns ``(from_dataset_id, to_dataset_id, relation, via_identity)``, where
        ``relation`` is one of :class:`Relation`. Resolved at query time from each
        dataset's declared ``SourceDatasets`` — order of ingest does not matter.
        """
        self._require_relations()
        return self._query("SELECT * FROM dataset_relations", [])

    def related_datasets(
        self, dataset_id: str, relation: Relation | str | None = None
    ) -> list[str]:
        """The dataset ids related to ``dataset_id`` by explicit provenance.

        ``relation`` optionally filters to one kind (e.g. :attr:`Relation.SHARES_SOURCE`).
        A shared-source link guarantees a shared subject/entity namespace, so a caller can
        then *soundly* match files across the boundary — bidslake resolves the dataset
        relation; the caller does the entity match::

            for other in lake.related_datasets(fp_id, relation=Relation.SHARES_SOURCE):
                lake.get(dataset_id=other, sub=f.sub, ses=f.ses, task=f.task, run=f.run)
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
        """The schema overlays applied when this database was indexed, as
        ``(index, source, sha256)`` in application order — empty if none.

        Augmented columns and tables are queryable with no extra step (``get`` and
        the table accessors validate against the live database), so this is for
        provenance/introspection. For *static* typing of augmented columns, generate
        a project-local module with ``python -m bidslake.stubgen``.
        """
        return self._lake.overlays()

    @property
    def term_maps(self) -> list[tuple[int, str, str]]:
        """The BEP-043 term maps applied when this database was indexed, as
        ``(index, source, sha256)`` in application order — empty if none.

        An adapter (``--adapter freesurfer``) projects a standardized *non-BIDS* dataset
        onto BIDS concepts via a term map, declares its tables via a BIDS overlay (see
        :attr:`overlays`), and its read/catalog policy via an ingestion schema (see
        :attr:`ingestion`). The resulting tables are queryable with no extra step
        (``lake.table("freesurfer_aparc")``); this is for provenance/introspection.
        """
        return self._lake.term_maps()

    @property
    def ingestion(self) -> list[tuple[int, str, str]]:
        """The ingestion schemas applied when this database was indexed, as
        ``(index, source, sha256)`` in application order — empty if none.
        """
        return self._lake.ingestion()

    def effective_schema(self) -> dict[str, Any] | None:
        """The full effective (base + overlays) BIDS schema stamped into the
        database, or ``None`` for a database that predates the stamp. Every database
        embeds its schema, so this recovers exactly what the catalog was built from."""
        raw = self._lake.effective_schema()
        return json.loads(raw) if raw is not None else None

    # -- the headline iterator --------------------------------------------

    def get(
        self,
        *,
        table: str = "scans",
        **filters: Unpack[GetFilters],
    ) -> Iterator[BidsFile]:
        """Yield :class:`BidsFile` for rows of ``table`` matching ``filters``.

        Each keyword is a column (BIDS entity, ``datatype``/``suffix``/
        ``extension``/``modality``, or ``dataset_id``). A scalar matches by
        equality, a sequence by ``IN (...)``, and ``None`` by ``IS NULL`` (so
        ``ses=None`` selects sessionless files). With no filters, iterates the
        whole table across every dataset in the database.

        Note: the result set is materialized in full (the Arrow-IPC buffer is read
        into a Polars frame) before any row is yielded, so peak memory is the whole
        result set — the generator form is for ergonomics, not streaming. Genuine
        streaming awaits the PyCapsule bridge (see ``src/lib.rs``).
        """
        where, params = self._compile_filters(table, filters)
        sql = f"SELECT * FROM {quote_ident(table)}"
        if where:
            sql += f" WHERE {where}"
        df = self._query(sql, params)
        for row in df.iter_rows(named=True):
            dataset_id = row["dataset_id"]
            file_path = row["file_path"]
            uri = self._resolve(dataset_id, file_path)
            yield BidsFile._from_row(dataset_id, file_path, uri, row, self)

    # -- escape hatch ------------------------------------------------------

    def sql(self, query: str | Template, params: Sequence[Any] | None = None) -> DataFrame:
        """Run raw SQL and return the result as Polars.

        Accepts either a plain SQL string (with optional positional ``params``)
        or a PEP 750 t-string, whose interpolations are lowered to DuckDB bind
        parameters — never string-concatenated — so values can't inject SQL::

            lake.sql(t"SELECT * FROM scans WHERE suffix = {suffix}")
        """
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
        """The ``{column_name: duckdb_type}`` mapping of ``table``."""
        return dict(self._columns(table))

    # -- internals ---------------------------------------------------------

    def _query(self, sql: str, params: list[Any]) -> DataFrame:
        return ipc_to_df(self._lake.query_ipc(sql, params))

    def _columns(self, table: str) -> dict[str, str]:
        cached = self._col_cache.get(table)
        if cached is None:
            cols = self._lake.columns(table)
            if not cols:
                raise KeyError(f"no table {table!r}; available: {sorted(self.tables())}")
            cached = dict(cols)
            self._col_cache[table] = cached
        return cached

    def _compile_filters(self, table: str, filters: Mapping[str, Any]) -> tuple[str, list[Any]]:
        cols = self._columns(table)
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

    def _resolve(self, dataset_id: str, file_path: str) -> str:
        root = self._effective_root(dataset_id)
        if root is None:
            return file_path
        return _bidslake.resolve_uri(root, file_path)

    def _effective_root(self, dataset_id: str) -> str | None:
        """The root URI to resolve `dataset_id`'s files against, honoring any
        `root_override` (per-dataset, wins) or `base_dir` (rebases every dataset
        under a new parent, keeping its original directory name)."""
        if dataset_id in self._root_override:
            return self._root_override[dataset_id]
        original = self._original_roots().get(dataset_id)
        if self._base_dir is not None and original is not None:
            name = original.rstrip("/").rsplit("/", 1)[-1]
            return f"{self._base_dir}/{name}"
        return original

    def _original_roots(self) -> dict[str, str]:
        if self._root_uris is None:
            df = self._query("SELECT dataset_id, root_uri FROM dataset_description", [])
            self._root_uris = dict(zip(df["dataset_id"], df["root_uri"], strict=True))
        return self._root_uris

    # -- BidsFile lazy lookups --------------------------------------------

    def _sidecar_metadata(self, dataset_id: str, file_path: str) -> dict[str, Any]:
        df = self._query(
            "SELECT * FROM sidecars WHERE dataset_id = ? AND file_path = ?",
            [dataset_id, file_path],
        )
        if df.height == 0:
            return {}
        row = df.row(0, named=True)
        # `other_data` holds custom (non-schema) fields in original BIDS case; the
        # typed columns hold the schema fields (also BIDS-cased). Merge both.
        meta: dict[str, Any] = {}
        other = row.get("other_data")
        if other:
            meta.update(json.loads(other))
        for key, value in row.items():
            if key in ("dataset_id", "file_path", "other_data"):
                continue
            if value is not None:
                meta[key] = value
        return meta

    def _events_for(self, dataset_id: str, file_path: str) -> DataFrame:
        return self._query(
            "SELECT e.* FROM file_associations fa "
            "JOIN events e ON e.dataset_id = fa.dataset_id "
            "AND e.file_path = fa.target_file_path "
            "WHERE fa.dataset_id = ? AND fa.source_file_path = ? "
            "AND fa.association_type = 'events' ORDER BY e.row_idx",
            [dataset_id, file_path],
        )

    def _associated_for(self, dataset_id: str, file_path: str, kind: str | None) -> list[BidsFile]:
        sql = (
            "SELECT target_file_path, association_type FROM file_associations "
            "WHERE dataset_id = ? AND source_file_path = ?"
        )
        params: list[Any] = [dataset_id, file_path]
        if kind is not None:
            sql += " AND association_type = ?"
            params.append(kind)
        df = self._query(sql, params)
        out: list[BidsFile] = []
        for row in df.iter_rows(named=True):
            target = row["target_file_path"]
            uri = self._resolve(dataset_id, target)
            # Entities aren't re-parsed here (the target may not be a scans row,
            # e.g. an events.tsv); callers get the path + association kind.
            out.append(
                BidsFile(
                    dataset_id=dataset_id,
                    file_path=target,
                    uri=uri,
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

        sidecar_sel = namespaced("sidecars", "sc", "sidecar__", {"dataset_id", "file_path"})
        participant_sel = namespaced("participants", "p", "participant__", {"dataset_id"})
        dataset_sel = namespaced("dataset_description", "dd", "dataset__", {"dataset_id"})
        parts = ["s.*", sidecar_sel, participant_sel, dataset_sel]
        select = ", ".join(p for p in parts if p)
        return (
            f"SELECT {select} FROM scans s "
            "LEFT JOIN sidecars sc ON sc.dataset_id = s.dataset_id AND sc.file_path = s.file_path "
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
        return f"BidsLake({self._path!r})"
