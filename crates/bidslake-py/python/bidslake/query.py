"""Composition helpers for statements built over the generated models.

`get()` answers "which files match these filters". A pipeline step usually asks something
else: "for each of these files, where are the *other* files that belong with it" — a
mask per run, one T1w per session, a FreeSurfer segmentation from a different dataset
entirely. Each sibling matches on a **different subset** of the anchor's entities, and
each has three possible outcomes rather than two: resolved, missing, or ambiguous.

`sibling` is that shape as one `LEFT JOIN LATERAL`. Everything else stays an ordinary
SQLAlchemy statement the caller writes and can print.

`sibling_path` and `unresolved` read back the columns `sibling` writes, so that every
consumer does not write the same unpack by hand.
"""

from __future__ import annotations

from collections.abc import Iterable, Mapping, Sequence
from typing import TYPE_CHECKING, Any, NamedTuple

from sqlalchemy import and_, func, select
from sqlalchemy.sql import ColumnElement, FromClause
from sqlalchemy.sql.selectable import LateralFromClause

from .schema.models import DatasetLinkTargets

if TYPE_CHECKING:
    from upath import UPath

    from .layout import BidsLake


class Sibling(NamedTuple):
    """What `sibling` returns: a lateral to join, and the columns to select.

    Attributes:
        lateral: `LEFT JOIN` this against the anchor, unconditionally —
            `frm.outerjoin(lat, true())`.
        columns: `<name>__dataset_id`, `__root_uri`, `__file_path`, `__n`.
    """

    lateral: LateralFromClause
    columns: list[ColumnElement[Any]]


def sibling(
    anchor: FromClause,
    name: str,
    join: Sequence[str] = (),
    where: Mapping[str, Any] | None = None,
    *,
    via: str | None = None,
) -> Sibling:
    """One file that belongs with each row of `anchor`, matched on `join`.

    The sibling is drawn from the *same relation* as the anchor, so a catalog whose overlay
    added columns (fMRIPrep's `from`/`to`, a FreeSurfer adapter's `seg`) needs nothing extra:
    alias the model `bidslake.stubgen` generated and both sides see those columns.

    One query serves the whole study, not one per role per unit; DuckDB decorrelates the
    laterals:

        a = AllFiles.__table__.alias("a")
        cols, frm = [a.c.sub, a.c.ses, a.c.dataset_id, a.c.root_uri, a.c.file_path], a
        for role, (join, where) in ROLES.items():
            lat, sel = sibling(a, role, join, where, via="fmriprep")
            cols += sel
            frm = frm.outerjoin(lat, true())
        units = lake.sql(select(*cols).select_from(frm).where(a.c.suffix == "bold"))

    Args:
        anchor: An alias of the registry — `AllFiles.__table__.alias("a")`.
        name: Prefixes the returned columns, and names the lateral.
        join: Entities the sibling must share with the anchor — the whole unit key for a
            per-run mask, `("sub", "ses")` for the session's T1w.
        where: Filters on the sibling itself. A value of `None` means `IS NULL`, which is how
            a native-space image is separated from its `space-*` resamplings. A mapping
            rather than `**kwargs` because `from` is a Python keyword and fMRIPrep uses it.
        via: Scopes the sibling to a *linked* dataset by the name this catalog gives it
            (`DatasetLinks`, or `bidslake link alias`), resolved in the **anchor's own**
            dataset. A hardcoded `dataset_id` cannot do this: ids are free text, and a study
            processed one subject at a time has one dataset per subject. Matching entities
            catalog-wide instead is unsound in a way `__n` cannot catch (ADR 0003).

    Returns:
        A `Sibling`: the lateral to join, and four columns to select. Three carry the
        sibling's `dataset_id`, `root_uri` and `file_path` — all of them, because a path only
        means something together with the root it was walked from. The fourth, `__n`, is the
        match count, and it is the point: a count rather than a raise, so `1` is resolved,
        `0` is missing (that subject is incomplete, which is data), and `2+` is ambiguous,
        meaning `join`/`where` under-specify and the answer must not be silently taken.
    """
    where = dict(where or {})
    f = getattr(anchor, "element", anchor).alias(f"f_{name}")

    # `all_files` is the whole registry, and every image has a `.json` sidecar carrying
    # identical entities — so without this every sibling matches two files and reads as
    # ambiguous. Overridable, because a sidecar is sometimes what you want.
    conds: list[ColumnElement[bool]] = []
    if "kind" not in where:
        conds.append(f.c.kind == "data")
    conds += [f.c[k].is_(None) if v is None else f.c[k] == v for k, v in where.items()]
    # `IS NOT DISTINCT FROM`, not `==`: a sessionless or single-run dataset has NULL
    # entities, and `NULL = NULL` is NULL, which would drop those units entirely.
    conds += [f.c[k].is_not_distinct_from(anchor.c[k]) for k in join]

    src: FromClause = f
    if via is not None:
        dl = DatasetLinkTargets.__table__.alias(f"dl_{name}")
        src = f.join(
            dl,
            and_(
                dl.c.from_dataset_id == anchor.c.dataset_id,
                dl.c.link_name == via,
                f.c.dataset_id == dl.c.target_dataset_id,
            ),
        )

    # Aggregated to one row so the lateral is scalar and the anchor keeps exactly one row
    # per unit whatever the match count. `list_extract(…, 1)` is DuckDB's 1-based index.
    lat = (
        select(
            func.list(f.c.dataset_id).label("d"),
            func.list(f.c.root_uri).label("r"),
            func.list(f.c.file_path).label("p"),
        )
        .select_from(src)
        .where(and_(*conds))
        .lateral(name)
    )
    return Sibling(
        lat,
        [
            func.list_extract(lat.c.d, 1).label(f"{name}__dataset_id"),
            func.list_extract(lat.c.r, 1).label(f"{name}__root_uri"),
            func.list_extract(lat.c.p, 1).label(f"{name}__file_path"),
            # `list()` over an empty lateral aggregates to NULL, not an empty list.
            func.coalesce(func.len(lat.c.p), 0).label(f"{name}__n"),
        ],
    )


def sibling_path(lake: BidsLake, row: Mapping[str, Any], name: str | None = None) -> UPath:
    """Where the sibling `name` of `row` is, or the anchor when `name` is None.

    The read side of `sibling`'s columns. A row carries
    `<name>__dataset_id`/`__root_uri`/`__file_path` per sibling because a path only means
    something together with the root it was walked from, and `BidsLake.resolve` is what
    applies `base_dir`/`root_override` to the pair.

    Args:
        lake: The catalog the row came from, which supplies `base_dir`/`root_override`.
        row: A row carrying the columns `sibling` writes.
        name: Which sibling to read. `None` reads the unprefixed columns — the anchor, when
            the caller selected them.

    Returns:
        A `upath.UPath`, like `BidsLake.resolve`. Wrap it in `bidslake.to_local_path` for a
        catalog you know is local.
    """
    p = f"{name}__" if name else ""
    return lake.resolve(row[f"{p}dataset_id"], row[f"{p}file_path"], row[f"{p}root_uri"])


def unresolved(row: Mapping[str, Any], names: Iterable[str]) -> dict[str, int]:
    """The siblings of `row` that are not exactly one file, as `{name: count}`.

    `0` is missing — that unit is incomplete — and `2+` is ambiguous, meaning the
    `join`/`where` given to `sibling` under-specify. Both are data rather than errors, and
    both are wrong to take silently, which is what this exists to make hard:

        if bad := unresolved(row, ROLES):
            log.warning(f"skipping {label}: {bad}")
            continue

    Args:
        row: A row carrying the `__n` column `sibling` writes for each name.
        names: The sibling names to check.

    Returns:
        Counts rather than a formatted reason, because callers phrase it differently and the
        difference between *missing* and *ambiguous* is often the difference between a
        subject to skip and a query to fix. An empty dict means every named sibling resolved.
    """
    return {n: c for n in names if (c := row[f"{n}__n"]) != 1}
