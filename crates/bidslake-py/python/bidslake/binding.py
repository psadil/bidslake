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
        anchor={"datatype": "func", "suffix": "bold", "desc": "preproc",
                "extension": ".nii.gz", "space": None},
        key=("sub", "ses", "task", "run"),
        inputs={
            "brain": FileInput(
                join=("sub", "ses", "task", "run"),
                where={"datatype": "func", "suffix": "mask", "desc": "brain",
                       "extension": ".nii.gz", "space": None}),
            "anat": FileInput(
                join=("sub", "ses"),
                where={"datatype": "anat", "suffix": "T1w", "desc": "preproc",
                       "extension": ".nii.gz", "space": None}),
            "wmparc": FileInput(
                join=("sub", "ses"), dataset_id="freesurfer",
                where={"seg": "wmparc", "extension": ".mgz"}),
            "motion": TableInput(
                association="fmriprep_confounds", table="fmriprep_confounds",
                columns=("rot_x", "rot_y", "rot_z", "trans_x", "trans_y", "trans_z"),
                order_by="row_idx"),
        },
    )

The ``motion`` input is keyed by ``association``, not by entities, and the difference is
not cosmetic. A confounds file happens to share ``sub``/``ses``/``task``/``run`` with the
images it describes, so an entity match gets the right answer — but only by luck of naming.
The same match is simply *wrong* for a describing file one directory level up: ds114's
root ``dwi.bval`` applies to twenty images and has no ``sub`` to join on at all. The
association is derived from the schema's own ``meta.associations``, so it is right in both
cases, and it is the same relation :meth:`BidsFile.get_described_by` reads
(``docs/adr/0007``). ``join=`` remains for tables with no declared association.

    for unit in lake.bind(MELODIC):
        if unit.unresolved:
            log.warning("skipping %s: %s", unit.key, unit.unresolved)
            continue
        run_the_pipeline(unit.anchor.local_path, unit.local("anat"),
                         unit.frame("motion"))

Two properties are the point. Resolution costs **one query per input**, not one per
input per unit, so the cost does not grow with the size of the study. And a unit
whose inputs do not resolve is *returned*, carrying :class:`Unresolved` entries that
say whether each was missing or ambiguous — so an incomplete subject is visible
before any work is submitted rather than fatal midway through it.

That holds *per unit*. Two whole-binding failures are not incompleteness and raise
``ValueError`` from :meth:`BidsLake.bind`: an anchor matching no files at all, and an
input resolving for zero of the units. Both mean a filter value that matches nothing
or a dataset that was never indexed, and both would otherwise read as a study in which
every subject is missing data.

A binding is only a query. It composes identically with a ``for`` loop, a process
pool, ``submitit.AutoExecutor.map_array``, a SLURM array job, or a Snakemake input
function; bidslake does not schedule anything.

Augmented catalogs
------------------
``Binding``, ``FileInput`` and ``TableInput`` check their filters against the BIDS
schema *this build ships*. A catalog carrying an overlay knows more words than that
— fMRIPrep's ``from``/``to`` entities and ``xfm`` suffix, a FreeSurfer adapter's
``seg`` — and a binding over them would be flagged key by key against a vocabulary
that does not contain them. They are therefore the generic :class:`BindingOf` /
:class:`FileInputOf` / :class:`TableInputOf` pinned to this build's ``GetFilters``
and ``Entity``; ``python -m bidslake.stubgen my.duckdb`` emits the same three names
pinned to *that catalog's* vocabulary, so importing them from the generated module
is the whole opt-in. The runtime classes are identical either way, and
:meth:`BidsLake.bind` accepts both.

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
from collections.abc import Mapping, Sequence
from pathlib import Path
from typing import TYPE_CHECKING, Any, Literal

from polars import DataFrame
from upath import UPath

from ._sql import DATAFILES, quote_ident
from .file import BidsFile
from .paths import to_local_path
from .schema._generated import Entity, GetFilters

if TYPE_CHECKING:
    from .layout import BidsLake


@dataclasses.dataclass(frozen=True, slots=True)
class FileInputOf[F: Mapping[str, Any], E: str]:
    """One sibling file, resolved per unit.

    ``join`` names the entities matched against the anchor — a *subset* of the
    binding's key, which is what lets an anatomical input match on ``(sub, ses)``
    while a functional one matches on ``(sub, ses, task, run)``. ``where`` takes the
    same filter vocabulary as :meth:`BidsLake.get`, including ``None`` for ``IS
    NULL`` (how a native-space image is distinguished from its ``space-*``
    resamplings).

    ``dataset_id`` scopes the search: one id, several, or ``None`` to search them
    all. A sequence is for a study sharded across datasets — one processing run per
    subject means one dataset per subject, so the FreeSurfer trees are
    ``sub-01-freesurfer``, ``sub-02-freesurfer``, … rather than one ``freesurfer``.

    Scoping is often unnecessary, and leaving it ``None`` is usually right: ``join``
    already narrows an input to its unit, and an input that then matches more than
    one file is reported as *ambiguous* rather than guessed. Reach for a scope when
    the join alone genuinely is not enough — two versions of the same derivative in
    one catalog, say — because naming ids couples the binding to a particular
    catalog's contents.

    The type parameters name the *vocabulary* the filters are checked against —
    see :class:`FileInput` below, which pins them to this build's BIDS schema.
    """

    join: tuple[E, ...]
    where: F
    dataset_id: str | Sequence[str] | None = None
    table: str = DATAFILES


@dataclasses.dataclass(frozen=True, slots=True)
class TableInputOf[E: str]:
    """A slice of an ingested table, resolved per unit.

    For the inputs that are not files at all — a few columns of a confounds table,
    the events for a run. ``order_by`` matters whenever row order is load-bearing.

    Order by ``row_idx`` for a table that has it: the column exists exactly on the tables
    whose source line order is meaningful (recordings, positional ``*timeseries.tsv``), and
    there it reproduces that order. Tables declared order-insensitive have no ``row_idx`` —
    ``events`` is the one BIDS table in that group, and its rows are addressed by ``onset``.

    Two ways to say which rows belong to a unit, and exactly one must be given:

    ``association``
        The rows of the file the schema says *describes* the anchor, through
        ``file_associations`` (docs/adr/0007). Derived, so it is right by construction —
        including for a describing file that sits at a *higher* directory level and covers
        many images, where there is no entity to match on at all.
    ``join``
        Match the anchor's entity values. A heuristic, and the only option for a table with
        no declared association. It happens to be right for confounds, whose file shares
        ``sub``/``ses``/``task``/``run`` with its images, and is simply wrong for an
        inherited one: a root-level ``dwi.bval`` has no ``sub`` to join on.
    """

    table: str
    columns: tuple[str, ...]
    join: tuple[E, ...] = ()
    association: str | None = None
    order_by: str | None = None

    def __post_init__(self) -> None:
        if bool(self.join) == bool(self.association):
            raise ValueError(
                f"TableInput({self.table!r}) needs exactly one of `join` or `association`; "
                f"got join={self.join!r}, association={self.association!r}"
            )


type InputOf[F: Mapping[str, Any], E: str] = FileInputOf[F, E] | TableInputOf[E]


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
class BindingOf[F: Mapping[str, Any], E: str]:
    """A declared unit of work: what anchors it, what identifies it, what it needs."""

    anchor: F
    key: tuple[E, ...]
    inputs: Mapping[str, InputOf[F, E]]
    table: str = DATAFILES


# The vocabulary a binding is checked against is the catalog's, not the library's:
# an overlay-augmented catalog knows entities (`from`, `to`) and suffixes (`xfm`)
# that the BIDS schema this build ships does not. The three names below pin the
# generic forms to *this build's* schema, so a plain BIDS user writes `FileInput`
# and `Binding` and gets every key and value checked with no ceremony; a user of an
# augmented catalog re-pins them to their own generated vocabulary, which is what
# `python -m bidslake.stubgen` emits (see its module docstring). They are subclasses
# rather than aliases so that `isinstance`, `dataclasses.fields`, and `repr` are
# unchanged — a subscripted generic cannot be used with `isinstance` at all, and an
# instance built from one is not an instance of these either, so any code branching
# on the public class would silently mishandle a catalog-pinned binding. `stubgen`
# emits subclasses for the same reason.


class FileInput(FileInputOf[GetFilters, Entity]):
    """:class:`FileInputOf`, pinned to the BIDS schema this build ships."""

    __slots__ = ()


class TableInput(TableInputOf[Entity]):
    """:class:`TableInputOf`, pinned to the BIDS schema this build ships."""

    __slots__ = ()


class Binding(BindingOf[GetFilters, Entity]):
    """:class:`BindingOf`, pinned to the BIDS schema this build ships."""

    __slots__ = ()


type Input = InputOf[GetFilters, Entity]


def _named_datasets(scope: str | Sequence[str] | None) -> tuple[str, ...]:
    """The dataset ids a scope names, as a tuple. A bare `str` is one name, not four
    characters — which is the bug a plain `tuple(scope)` would introduce."""
    if scope is None:
        return ()
    return (scope,) if isinstance(scope, str) else tuple(scope)


def _check_scopes(lake: BidsLake, binding: BindingOf[Any, Any]) -> None:
    """Reject a `dataset_id` naming a dataset the catalog does not have.

    Checked up front rather than after resolution, because a scope can be *partly*
    wrong: `["ds001", "typo"]` still resolves from `ds001`, so a late check sees a
    working input and the misspelled half is silently dropped from the search. Ids
    are free text, and a study indexed one subject at a time has one dataset per
    subject, so a name that looks obvious is often not the one in the catalog.
    """
    known = _dataset_ids(lake)
    for name, spec in binding.inputs.items():
        if not isinstance(spec, FileInputOf):
            continue
        absent = sorted(set(_named_datasets(spec.dataset_id)) - known)
        if not absent:
            continue
        msg = (
            f"input {name!r} names dataset(s) {', '.join(map(repr, absent))}, which "
            f"are not in this catalog. It holds: {', '.join(sorted(known)) or '(none)'}. "
            f"Dataset ids are free text — a study indexed one subject at a time has one "
            f"dataset per subject — so either name them all, or drop `dataset_id` and "
            f"let the join on {list(spec.join)} scope the input."
        )
        raise ValueError(msg)


def _dataset_ids(lake: BidsLake) -> set[str]:
    """Every `dataset_id` in the catalog, for naming what a bad scope could have meant.

    From `dataset_roots` rather than the file registry: a dataset is registered when it is
    named, so this holds even for one indexed from a root that turned out to be empty —
    which is exactly the catalog a misspelled scope is most likely to be pointed at.
    """
    return set(lake._query("SELECT DISTINCT dataset_id FROM dataset_roots", [])["dataset_id"])


def _check_columns(lake: BidsLake, table: str, names: Sequence[str], what: str) -> None:
    """Fail before any query runs, naming the table — a missing join entity is a
    typo far more often than it is a real absence, and the SQL error for it is
    unreadable."""
    # The reachable columns, not the stored ones: a file-keyed table's BIDS concepts live
    # on the registry and are joined in (docs/adr/0006), so joining on `sub` is legal
    # against `events` even though `events` does not store it.
    cols = lake._filter_columns(table)
    missing = [n for n in names if n not in cols]
    if missing:
        msg = f"{what}: column(s) {missing} not in table {table!r}; available: {sorted(cols)}"
        raise KeyError(msg)


def _index_files(
    lake: BidsLake, name: str, spec: FileInputOf[Any, Any]
) -> dict[tuple[Any, ...], list[tuple[str, str, str]]]:
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
    # `root_uri` travels with the row: a dataset may span several ingest roots, and which
    # one a file came from is what says where to open it (docs/adr/0005).
    cols = ", ".join(quote_ident(c) for c in (*spec.join, "dataset_id", "root_uri", "file_path"))
    sql = f"SELECT {cols} FROM {lake._relation(spec.table)}"
    if clause:
        sql += f" WHERE {clause}"

    index: dict[tuple[Any, ...], list[tuple[str, str, str]]] = {}
    for row in lake._query(sql, params).iter_rows(named=True):
        key = tuple(row[c] for c in spec.join)
        index.setdefault(key, []).append((row["dataset_id"], row["root_uri"], row["file_path"]))
    return index


def _index_table_by_association(
    lake: BidsLake, name: str, spec: TableInputOf[Any]
) -> dict[int, DataFrame]:
    """Every row of one table input, bucketed by the **anchor file id** it describes.

    Still one query for the whole study, like `_index_table`: the edges are a join, not a
    per-unit lookup. The bucketing key is `file_associations.source_file_id`, so a
    describing file shared by many anchors lands in each of their buckets from one stored
    copy.

    Columns are checked against the raw table, not `lake._relation(...)`: the SELECT is off
    the table itself, so the registry's concept columns are not in scope here.
    """
    assert spec.association is not None
    _check_columns(lake, spec.table, spec.columns, f"input {name!r}")
    if spec.order_by is not None:
        _check_columns(lake, spec.table, [spec.order_by], f"input {name!r} order_by")
    select = ", ".join(f"t.{quote_ident(c)}" for c in spec.columns)
    sql = (
        f"SELECT fa.source_file_id AS __src, {select} "
        f"FROM file_associations fa "
        f"JOIN {quote_ident(spec.table)} t ON t.file_id = fa.target_file_id "
        f"WHERE fa.association_type = ?"
    )
    if spec.order_by is not None:
        sql += f" ORDER BY t.{quote_ident(spec.order_by)}"

    df = lake._query(sql, [spec.association])
    if df.is_empty():
        return {}
    parts = df.partition_by(["__src"], as_dict=True, maintain_order=True)
    return {key[0]: part.select(list(spec.columns)) for key, part in parts.items()}


def _index_table(
    lake: BidsLake, name: str, spec: TableInputOf[Any]
) -> dict[tuple[Any, ...], DataFrame]:
    """Every row of one table input, bucketed by its join key, ordered if asked."""
    _check_columns(lake, spec.table, (*spec.join, *spec.columns), f"input {name!r}")
    if spec.order_by is not None:
        _check_columns(lake, spec.table, [spec.order_by], f"input {name!r} order_by")
    select = ", ".join(quote_ident(c) for c in (*spec.join, *spec.columns))
    sql = f"SELECT {select} FROM {lake._relation(spec.table)}"
    if spec.order_by is not None:
        sql += f" ORDER BY {quote_ident(spec.order_by)}"

    df = lake._query(sql, [])
    if df.is_empty():
        return {}
    # `maintain_order` keeps the ORDER BY above meaningful within each partition.
    parts = df.partition_by(list(spec.join), as_dict=True, maintain_order=True)
    return {key: part.select(list(spec.columns)) for key, part in parts.items()}


def resolve(lake: BidsLake, binding: BindingOf[Any, Any]) -> list[Unit]:
    """Resolve `binding` against `lake`, one :class:`Unit` per anchor file.

    Takes the *generic* `BindingOf`, so a binding pinned to a generated catalog
    vocabulary resolves exactly as one pinned to the shipped schema — the runtime
    classes are identical, and only the checked vocabulary differs.

    Eager, and returns a `list` rather than a generator so that it *is*: a malformed
    binding and every unresolved input surface when this is called, not on the first
    iteration. That distinction is the whole point — "incomplete subjects are visible
    before you submit anything" is not true of a generator nobody has started yet, and
    a plan you can count and inspect is what makes it worth building.

    A study's worth of units is small (hundreds of rows across a handful of inputs),
    so materializing them costs nothing worth saving.
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

    _check_scopes(lake, binding)

    file_specs = {n: s for n, s in binding.inputs.items() if isinstance(s, FileInputOf)}
    table_specs = {n: s for n, s in binding.inputs.items() if isinstance(s, TableInputOf)}
    file_index = {n: _index_files(lake, n, s) for n, s in file_specs.items()}
    # Association-keyed inputs bucket by the anchor's own `file_id`; entity-keyed ones by
    # its entity tuple. Both are one query for the whole study.
    table_index: dict[str, dict[Any, DataFrame]] = {
        n: (
            _index_table_by_association(lake, n, s)
            if s.association is not None
            else _index_table(lake, n, s)
        )
        for n, s in table_specs.items()
    }

    units: list[Unit] = []
    for anchor in lake.get(table=binding.table, **binding.anchor):
        key = tuple(anchor.entities.get(k) for k in binding.key)
        resolved: dict[str, UPath | DataFrame] = {}
        unresolved: list[Unresolved] = []

        for name, spec in file_specs.items():
            hits = file_index[name].get(tuple(anchor.entities.get(k) for k in spec.join), [])
            if len(hits) == 1:
                dataset_id, root_uri, file_path = hits[0]
                resolved[name] = lake.resolve(dataset_id, file_path, root_uri)
            else:
                unresolved.append(
                    Unresolved(name, len(hits), "missing" if not hits else "ambiguous")
                )

        for name, spec in table_specs.items():
            bucket = (
                anchor.file_id
                if spec.association is not None
                else tuple(anchor.entities.get(k) for k in spec.join)
            )
            part = table_index[name].get(bucket)
            if part is None or part.is_empty():
                unresolved.append(Unresolved(name, 0, "missing"))
            else:
                resolved[name] = part

        units.append(
            Unit(
                key=key,
                entities=anchor.entities,
                anchor=anchor,
                inputs=resolved,
                unresolved=tuple(unresolved),
            )
        )

    _check_productive(lake, binding, units)
    return units


def _check_productive(lake: BidsLake, binding: BindingOf[Any, Any], units: list[Unit]) -> None:
    """Refuse a binding that resolves *nothing*, which is never incompleteness.

    Per-unit gaps are data — that is the whole design — but two shapes are not:

    An anchor matching no files yields an empty list, so ``for unit in lake.bind(B)``
    silently does no work. A wrong *key* raises loudly, but a wrong *value*
    (``suffix="notasuffix"``) does not, and this is where it would otherwise land.

    An input resolving for *zero of N* units is reported today as N incomplete
    subjects, which reads exactly like real missing data and hides the actual cause:
    a filter that matches nothing, or a dataset that was never indexed. Both are
    setup errors, and both are worth stopping for — whereas an input that resolves
    for *some* units is genuinely incomplete data and stays a :class:`Unresolved`.
    """
    if not units:
        msg = (
            f"binding anchor matched no files: {dict(binding.anchor)}. Every filter key "
            f"is valid, so this is a value that matches nothing — check the values, and "
            f"that the dataset is indexed into this catalog."
        )
        raise ValueError(msg)

    for name, spec in binding.inputs.items():
        if any(name in u.inputs for u in units):
            continue
        # Never resolving is only a *setup* error when the filter matched nothing at
        # all. An input that was ambiguous everywhere also resolves for zero units,
        # but its filter matches plenty — too much — which is a different fault with
        # a different fix, already reported per unit.
        matched = {x.n_matched for u in units for x in u.unresolved if x.name == name}
        if matched != {0}:
            continue
        if isinstance(spec, FileInputOf):
            # Any named dataset exists — `_check_scopes` ran first — so the scope is
            # still worth printing: the filter can match nothing merely because it was
            # narrowed to datasets that happen not to hold this kind of file.
            what = f"filter {dict(spec.where)}"
            if spec.dataset_id is not None:
                what += f" in dataset {spec.dataset_id!r}"
        else:
            what = f"columns {list(spec.columns)} of table {spec.table!r}"
        msg = (
            f"input {name!r} matched nothing for any of {len(units)} units, so it is a "
            f"binding or indexing problem rather than incomplete data: its {what} "
            f"joined on {list(spec.join)} found no candidates at all. Check the filter "
            f"values, and that the dataset it reads is indexed into this catalog. An "
            f"input that resolves for *some* units is genuinely incomplete data and is "
            f"reported per unit instead."
        )
        raise ValueError(msg)
