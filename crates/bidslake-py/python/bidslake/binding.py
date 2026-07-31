"""Bindings: declare the units of work a pipeline runs over, and what each consumes.

A pipeline step almost never operates on *one* file. It operates on a unit — a
subject, a session, a run — and needs a handful of sibling files that belong to that
unit, each matched on a *different* subset of its entities. A denoising step might
anchor on a native-space preproc BOLD and need the brain mask for the same
``(sub, ses, task, run)``, the T1w for the same ``(sub, ses)`` only, a FreeSurfer
segmentation from a *different dataset*, and six motion columns out of an ingested
confounds table.

Written by hand that is one query for the anchor plus one per sibling *per unit*,
each wrapped in a "there must be exactly one" check that raises. The raise is the
real problem: a subject missing one input aborts the loop at whatever hour it is
reached, after everything before it has already been computed.

A binding states that shape once::

    MELODIC = Binding(
        anchor=GetFilters(datatype="func", suffix="bold", desc="preproc",
                          extension=".nii.gz", space=None),
        key=("sub", "ses", "task", "run"),
        inputs={
            "brain": FileInput(
                join=("sub", "ses", "task", "run"),
                where=GetFilters(datatype="func", suffix="mask", desc="brain",
                                 extension=".nii.gz", space=None)),
            "anat": FileInput(
                join=("sub", "ses"),
                where=GetFilters(datatype="anat", suffix="T1w", desc="preproc",
                                 extension=".nii.gz", space=None)),
            "wmparc": FileInput(
                join=("sub", "ses"), dataset_id="freesurfer",
                where=GetFilters(seg="wmparc", extension=".mgz")),
            "motion": TableInput(
                join=("sub", "ses", "task", "run"), table="fmriprep_confounds",
                columns=("rot_x", "rot_y", "rot_z", "trans_x", "trans_y", "trans_z"),
                order_by="row_idx"),
        },
    )

    for unit in lake.bind(MELODIC):
        if unit.unresolved:
            log.warning("skipping %s: %s", unit.key, unit.unresolved)
            continue
        run_the_pipeline(unit.anchor.local_path, unit.inputs["anat"], ...)

Two properties are the point. Resolution costs **one query per input**, not one per
input per unit, so the cost does not grow with the size of the study. And a unit
whose inputs do not resolve is *returned*, carrying :class:`Unresolved` entries that
say whether each was missing or ambiguous — so an incomplete subject is visible
before any work is submitted rather than fatal midway through it.

A binding is only a query. It composes identically with a ``for`` loop, a process
pool, ``submitit.AutoExecutor.map_array``, a SLURM array job, or a Snakemake input
function; bidslake does not schedule anything.

Staged on purpose
-----------------
This is deliberately **typed Python, not a JSON artifact**, for now. The dataclasses
below are a 1:1 match for what such an artifact would hold, so promoting them later
is writing a serializer rather than a redesign — at which point a binding could be
stamped into the catalog and travel with the data, as overlays, term maps, and
ingestion fragments already do.

It is not that artifact yet because the shape has not earned its metaschema. ADR
0002 §1 records why the ``x_bidslake`` prototype was rejected — "one artifact, three
concerns, no standards path, bespoke parser" — and a declarative work-unit language
invented before two or three real pipelines have exercised it would be the same
mistake. Promote it once the shape stops moving; the trigger is recorded in
``TODO.md`` under "Derivation layer".
"""

from __future__ import annotations

import dataclasses
from collections.abc import Iterator, Mapping, Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal

from polars import DataFrame
from upath import UPath

from ._sql import quote_ident
from .file import BidsFile
from .paths import to_local_path
from .schema._generated import Entity, GetFilters

if TYPE_CHECKING:
    from .layout import BidsLake


@dataclasses.dataclass(frozen=True, slots=True)
class FileInput:
    """One sibling file, resolved per unit.

    ``join`` names the entities matched against the anchor — a *subset* of the
    binding's key, which is what lets an anatomical input match on ``(sub, ses)``
    while a functional one matches on ``(sub, ses, task, run)``. ``where`` takes the
    same filter vocabulary as :meth:`BidsLake.get`, including ``None`` for ``IS
    NULL`` (how a native-space image is distinguished from its ``space-*``
    resamplings).

    ``dataset_id`` scopes the search to one dataset; ``None`` searches them all,
    which is usually what you want when a study is one catalog of several datasets.
    """

    join: tuple[Entity, ...]
    where: GetFilters
    dataset_id: str | None = None
    table: str = "scans"


@dataclasses.dataclass(frozen=True, slots=True)
class TableInput:
    """A slice of an ingested table, resolved per unit.

    For the inputs that are not files at all — a few columns of a confounds table,
    the events for a run. ``order_by`` matters whenever row order is load-bearing
    (``row_idx`` preserves the original TSV order).
    """

    join: tuple[Entity, ...]
    table: str
    columns: tuple[str, ...]
    order_by: str | None = None


type Input = FileInput | TableInput


@dataclasses.dataclass(frozen=True, slots=True)
class Unresolved:
    """Why one named input did not resolve to exactly one thing.

    ``n_matched`` and ``reason`` are kept apart because the two failures want
    different fixes: *missing* means the unit is incomplete (a pipeline did not run,
    or its outputs were not indexed), while *ambiguous* means the binding under-
    specifies (a filter needs narrowing). Collapsing both into "not found" is what
    makes the hand-written version's error message unhelpful.
    """

    name: str
    n_matched: int
    reason: Literal["missing", "ambiguous"]


@dataclasses.dataclass(frozen=True, slots=True)
class Unit:
    """One unit of work: the anchor file, its resolved inputs, and what is missing."""

    key: tuple[str | None, ...]
    entities: Mapping[str, Any]
    anchor: BidsFile
    inputs: Mapping[str, UPath | DataFrame]
    unresolved: tuple[Unresolved, ...]

    def local(self, name: str) -> Path:
        """The on-disk path of file input ``name``, for handing to a local tool.

        ``inputs`` holds :class:`~upath.UPath` handles, which work for local and
        remote alike — but a ``UPath`` stringifies back to a *URI*
        (``file:///data/…``), so passing one to a subprocess, nibabel, or anything
        else expecting a filename silently produces a path no tool can open. This is
        the same distinction :attr:`BidsFile.path` and :attr:`BidsFile.local_path`
        draw, and it raises :class:`RemotePathError` on a remote dataset rather than
        handing back something broken.
        """
        value = self.inputs[name]
        if isinstance(value, DataFrame):
            msg = f"input {name!r} is a table slice, not a file"
            raise TypeError(msg)
        return to_local_path(str(value))

    def frame(self, name: str) -> DataFrame:
        """The rows of table input ``name``.

        The counterpart of :meth:`local`. ``inputs`` is typed
        ``UPath | DataFrame`` because a binding mixes both kinds, so reading a table
        slice straight out of it hands a checker a union that will not satisfy a
        ``DataFrame`` parameter. Narrowing here keeps that out of every call site.
        """
        value = self.inputs[name]
        if not isinstance(value, DataFrame):
            msg = f"input {name!r} is a file, not a table slice"
            raise TypeError(msg)
        return value

    def entity(self, name: str) -> str:
        """One of this unit's key entities, required to be present.

        ``key`` and ``entities`` are both ``str | None``-valued, because an entity is
        legitimately absent for a sessionless or single-run dataset. Where a caller
        *requires* one — it is about to build a filename from it — this narrows and
        fails loudly rather than letting a ``None`` reach a path join and produce
        ``sub-None``.
        """
        value = self.entities.get(name)
        if value is None:
            msg = (
                f"unit {self.key} has no {name!r}; it is absent for this dataset, so "
                f"read it from `entities` and handle None if that is expected"
            )
            raise KeyError(msg)
        return str(value)

    def __repr__(self) -> str:
        state = f", unresolved={len(self.unresolved)}" if self.unresolved else ""
        return f"Unit(key={self.key}{state})"


@dataclasses.dataclass(frozen=True, slots=True)
class Binding:
    """A declared unit of work: what anchors it, what identifies it, what it needs."""

    anchor: GetFilters
    key: tuple[Entity, ...]
    inputs: Mapping[str, Input]
    table: str = "scans"


def _check_columns(lake: BidsLake, table: str, names: Sequence[str], what: str) -> None:
    """Fail before any query runs, naming the table — a missing join entity is a
    typo far more often than it is a real absence, and the SQL error for it is
    unreadable."""
    cols = lake.columns(table)
    missing = [n for n in names if n not in cols]
    if missing:
        msg = f"{what}: column(s) {missing} not in table {table!r}; available: {sorted(cols)}"
        raise KeyError(msg)


def _index_files(
    lake: BidsLake, name: str, spec: FileInput
) -> dict[tuple[Any, ...], list[tuple[str, str]]]:
    """Every candidate for one file input, bucketed by its join key.

    One query for the whole study, not one per unit: the join happens here, in a
    dict keyed by the tuple of join values. Tuple equality also gives ``NULL ==
    NULL`` for free, which is what a sessionless dataset needs and what a SQL join
    would not do without ``join_nulls``.
    """
    _check_columns(lake, spec.table, spec.join, f"input {name!r} join")
    where = dict(spec.where)
    if spec.dataset_id is not None:
        where["dataset_id"] = spec.dataset_id
    clause, params = lake._compile_filters(spec.table, where)
    cols = ", ".join(quote_ident(c) for c in (*spec.join, "dataset_id", "file_path"))
    sql = f"SELECT {cols} FROM {quote_ident(spec.table)}"
    if clause:
        sql += f" WHERE {clause}"

    index: dict[tuple[Any, ...], list[tuple[str, str]]] = {}
    for row in lake._query(sql, params).iter_rows(named=True):
        key = tuple(row[c] for c in spec.join)
        index.setdefault(key, []).append((row["dataset_id"], row["file_path"]))
    return index


def _index_table(lake: BidsLake, name: str, spec: TableInput) -> dict[tuple[Any, ...], DataFrame]:
    """Every row of one table input, bucketed by its join key, ordered if asked."""
    _check_columns(lake, spec.table, (*spec.join, *spec.columns), f"input {name!r}")
    if spec.order_by is not None:
        _check_columns(lake, spec.table, [spec.order_by], f"input {name!r} order_by")
    select = ", ".join(quote_ident(c) for c in (*spec.join, *spec.columns))
    sql = f"SELECT {select} FROM {quote_ident(spec.table)}"
    if spec.order_by is not None:
        sql += f" ORDER BY {quote_ident(spec.order_by)}"

    df = lake._query(sql, [])
    if df.is_empty():
        return {}
    # `maintain_order` keeps the ORDER BY above meaningful within each partition.
    parts = df.partition_by(list(spec.join), as_dict=True, maintain_order=True)
    return {key: part.select(list(spec.columns)) for key, part in parts.items()}


def resolve(lake: BidsLake, binding: Binding) -> Iterator[Unit]:
    """Resolve `binding` against `lake`, yielding one :class:`Unit` per anchor file.

    Eager: every query runs before the first unit is yielded. A study's worth of
    units is small (hundreds of rows across a handful of inputs) and materializing
    them is what makes the unresolved ones visible up front, which is the point.
    """
    _check_columns(lake, binding.table, binding.key, "binding key")
    for name, spec in binding.inputs.items():
        extra = set(spec.join) - set(binding.key)
        if extra:
            msg = (
                f"input {name!r} joins on {sorted(extra)}, which the binding key "
                f"{list(binding.key)} does not provide"
            )
            raise KeyError(msg)

    file_specs = {n: s for n, s in binding.inputs.items() if isinstance(s, FileInput)}
    table_specs = {n: s for n, s in binding.inputs.items() if isinstance(s, TableInput)}
    file_index = {n: _index_files(lake, n, s) for n, s in file_specs.items()}
    table_index = {n: _index_table(lake, n, s) for n, s in table_specs.items()}

    for anchor in lake.get(table=binding.table, **binding.anchor):
        key = tuple(anchor.entities.get(k) for k in binding.key)
        resolved: dict[str, UPath | DataFrame] = {}
        unresolved: list[Unresolved] = []

        for name, spec in file_specs.items():
            hits = file_index[name].get(tuple(anchor.entities.get(k) for k in spec.join), [])
            if len(hits) == 1:
                dataset_id, file_path = hits[0]
                resolved[name] = lake.resolve(dataset_id, file_path)
            else:
                unresolved.append(
                    Unresolved(name, len(hits), "missing" if not hits else "ambiguous")
                )

        for name, spec in table_specs.items():
            part = table_index[name].get(tuple(anchor.entities.get(k) for k in spec.join))
            if part is None or part.is_empty():
                unresolved.append(Unresolved(name, 0, "missing"))
            else:
                resolved[name] = part

        yield Unit(
            key=key,
            entities=anchor.entities,
            anchor=anchor,
            inputs=resolved,
            unresolved=tuple(unresolved),
        )
