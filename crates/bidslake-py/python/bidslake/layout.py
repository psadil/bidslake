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
from upath import UPath

from . import _bidslake
from ._arrow import ipc_to_df
from ._lazy import build_lazy
from ._sql import ALL_FILES, DATAFILES, quote_ident
from .binding import BindingOf, Unit, resolve
from .file import BidsFile
from .paths import to_upath, to_uri
from .relations import Relation
from .schema._generated import SCHEMA_VERSION, GetFilters

# Selected from rather than named directly so a caller's own `WHERE` composes with the
# `kind` filter that narrows it to data files.
_ALL_FILES_SQL = f"SELECT * FROM {ALL_FILES}"


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
        self._root_uris: dict[str, list[str]] | None = None
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
    def datafiles(self) -> Table:
        """One row per primary **data file**, with its BIDS concepts.

        The surface :meth:`get` iterates. A filter over :attr:`all_files` rather than a
        table of its own — "data file" is a `kind`, and narrowing to it is a `WHERE`
        (docs/adr/0006).
        """
        return Table(self, DATAFILES, sql=f"{_ALL_FILES_SQL} WHERE kind = 'data'")

    @property
    def all_files(self) -> Table:
        """One row per file the walk saw — every kind, with its BIDS concepts.

        The dataset's manifest: data files, sidecars, tabular files, gradients, and the
        documentation a dataset ships. Every file-keyed table joins this on ``file_id``.
        """
        return Table(self, ALL_FILES)

    @property
    def scans(self) -> Table:
        """The BIDS ``scans.tsv`` table: acquisition metadata per data file.

        Not the file registry — that is :attr:`all_files`. Keyed by ``file_id``, so join it
        to :attr:`datafiles` to see which file a row is about.
        """
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
        """One row per data file, widened with sidecar/participant/dataset columns.

        Hides the joins (sidecars and scans by ``file_id``; files↔participants by
        ``sub``/path-prefix). Joined-table columns are namespaced
        ``sidecar__*``/``participant__*``/``dataset__*``/``scan__*`` (BIDS's own ``__``
        convention) so they never collide with the registry's own columns.
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

    def resolve(self, dataset_id: str, file_path: str, root_uri: str | None = None) -> UPath:
        """One handle that opens the file a registry row names.

        The same resolution :attr:`BidsFile.path` uses — honoring ``base_dir`` and
        ``root_override`` — but for any row in any table, not just ``all_files``.

        ``root_uri`` is the root the file was walked from. It is optional only because a
        single-root dataset has nothing to disambiguate; pass it (every file-keyed row
        can reach it through ``file_registry``) for a dataset that spans several roots,
        which is what a study processed one subject at a time falls into (docs/adr/0005).

        Its main use is reading what the catalog deliberately did not store. A table
        whose ingestion policy is ``undeclared: catalog`` keeps only the columns its
        schema declares; the file on disk stays the record of the rest, and the file
        registry is the index of those files::

            row = (lake.all_files.pl()
                   .filter(pl.col("file_path").str.contains("confounds.tsv"))
                   .row(0, named=True))
            path = lake.resolve(row["dataset_id"], row["file_path"], row["root_uri"])
            full = pl.read_csv(path.open("rb"), separator="\\t", null_values="n/a")
            full.select("^a_comp_cor_.*$")   # the columns the catalog did not store

        Read through ``.open()`` rather than ``str(path)``: a :class:`UPath` stringifies
        back to a URI (scheme and all), and it is what keeps the recipe working for a
        remote dataset.

        To learn which names those are without touching disk, read
        ``tabular_undeclared_columns``.
        """
        return to_upath(self._resolve(dataset_id, root_uri, file_path))

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
        table: str = DATAFILES,
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

    def bind(self, binding: BindingOf[Any, Any]) -> list[Unit]:
        """Resolve a :class:`~bidslake.binding.Binding` into units of work.

        Returns one :class:`~bidslake.binding.Unit` per anchor file, each carrying its
        resolved inputs and a tuple of :class:`~bidslake.binding.Unresolved` entries
        for the inputs that did not match exactly one thing — a per-unit gap is data,
        not an exception. Costs one query per declared input, not one per input per
        unit::

            for unit in lake.bind(MELODIC):
                if unit.unresolved:
                    continue
                work(unit.anchor.local_path, unit.local("anat"))

        Raises :class:`ValueError` for the two shapes that are never incomplete data:
        an anchor matching no files at all, and an input resolving for *zero* units.
        Both mean a filter value that matches nothing or a dataset that was never
        indexed, rather than a missing subject.

        See :mod:`bidslake.binding` for the declaration format and why it is typed
        Python rather than a stamped JSON artifact.
        """
        return resolve(self, binding)

    # -- escape hatch ------------------------------------------------------

    def sql(self, query: str | Template, params: Sequence[Any] | None = None) -> DataFrame:
        """Run raw SQL and return the result as Polars.

        Accepts either a plain SQL string (with optional positional ``params``)
        or a PEP 750 t-string, whose interpolations are lowered to DuckDB bind
        parameters — never string-concatenated — so values can't inject SQL::

            lake.sql(t"SELECT * FROM all_files WHERE suffix = {suffix}")
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
            # `datafiles` is `all_files` narrowed by `kind`, not an object in the database,
            # so it has no columns of its own to introspect — it has exactly the view's.
            cols = self._lake.columns(ALL_FILES if table == DATAFILES else table)
            if not cols:
                raise KeyError(f"no table {table!r}; available: {sorted(self.tables())}")
            cached = dict(cols)
            self._col_cache[table] = cached
        return cached

    def _relation(self, table: str) -> str:
        """`table` as a FROM item, widened with the file registry where it needs to be.

        Three shapes. `datafiles` is a `kind` filter over `all_files` rather than an object
        in the database, so it is spelled out. A file-keyed satellite (`scans`, `events`,
        `sidecars`, an adapter's tables) holds a `file_id` and its own measurements — the
        BIDS concepts live once, on the registry, instead of being copied onto all two
        dozen of them (docs/adr/0006) — so it is joined back to them. Everything else,
        including the registry itself and the entity-keyed tables, stands alone.
        """
        if table == DATAFILES:
            return f"({_ALL_FILES_SQL} WHERE kind = 'data')"
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
        """Every column a query against `table` may name — its own, plus the registry's
        for a file-keyed one, which :meth:`_relation` reaches by joining `file_id`.

        Kept apart from :meth:`columns`, which stays a faithful report of what the
        database holds.
        """
        own = self._columns(table)
        if table in (DATAFILES, ALL_FILES) or "file_id" not in own:
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
        """The root URI to resolve a file against, honoring any `root_override` or
        `base_dir` (which rebases under a new parent, keeping the directory name).

        `root_uri` is the file's own — a dataset may have several (docs/adr/0005), and
        which one a file belongs to is a property of the file, so it travels with the row
        rather than being looked up per dataset. `None` for a caller that has only a
        dataset, which then resolves only if that dataset has exactly one root.
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
        # `file_id` is the join key, not a metadata field. It replaced the
        # `(dataset_id, file_path)` pair this skipped before (docs/adr/0006); leaving the old
        # names here let the key itself through as a `Decimal` beside `RepetitionTime`.
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

        The one shape every such relation takes (docs/adr/0007): the rows are stored once,
        keyed by the describing file, and the edge says which data files they are about. An
        inherited describing file — ds114's root `task-*_events.tsv` over 20 BOLD runs, or
        its `dwi.bval` over 20 images — is one stored copy and N edges, so this is a lookup
        rather than a scan over duplicated rows.

        `association_type` names the table by default: a schema-declared edge is named for
        the table it feeds (`events`), and an overlay-declared one is namespaced to it
        (`fmriprep_confounds`), so the map is the identity in both cases.
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
            f"SELECT {select} FROM ({_ALL_FILES_SQL} WHERE kind = 'data') s "
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
        return f"BidsLake({self._path!r})"
