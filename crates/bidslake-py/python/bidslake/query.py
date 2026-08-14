"""Composition helpers for statements built over the generated models.

`get()` answers "which files match these filters". A pipeline step usually asks something
else: "for each of these files, where are the *other* files that belong with it" — a
mask per run, one T1w per session, a FreeSurfer segmentation from a different dataset
entirely. Each sibling matches on a **different subset** of the anchor's entities, and
each has three possible outcomes rather than two: resolved, missing, or ambiguous.

:func:`sibling` is that shape as one `LEFT JOIN LATERAL`. Everything else stays an
ordinary SQLAlchemy statement the caller writes and can print.
"""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Any, NamedTuple

from sqlalchemy import and_, func, select
from sqlalchemy.sql import ColumnElement, FromClause
from sqlalchemy.sql.selectable import LateralFromClause

from .schema.models import DatasetLinkTargets


class Sibling(NamedTuple):
    """What :func:`sibling` returns: a lateral to join, and the columns to select."""

    #: `LEFT JOIN` this against the anchor, unconditionally — ``frm.outerjoin(lat, true())``.
    lateral: LateralFromClause
    #: ``<name>__dataset_id``, ``__root_uri``, ``__file_path``, ``__n``.
    columns: list[ColumnElement[Any]]


def sibling(
    anchor: FromClause,
    name: str,
    join: Sequence[str] = (),
    where: Mapping[str, Any] | None = None,
    *,
    via: str | None = None,
) -> Sibling:
    """One file that belongs with each row of ``anchor``, matched on ``join``.

    ``anchor`` is an alias of the registry — ``AllFiles.__table__.alias("a")`` — and the
    sibling is drawn from *that same relation*, so a catalog whose overlay added columns
    (fMRIPrep's ``from``/``to``, a FreeSurfer adapter's ``seg``) needs nothing extra:
    alias the model :mod:`~bidslake.stubgen` generated and both sides see those columns.

    ``join`` names the entities the sibling must share with the anchor — the whole unit
    key for a per-run mask, ``("sub", "ses")`` for the session's T1w. ``where`` filters
    the sibling itself; a value of ``None`` means ``IS NULL``, which is how a
    native-space image is separated from its ``space-*`` resamplings. It is a mapping
    rather than ``**kwargs`` because ``from`` is a Python keyword and fMRIPrep uses it.

    ``via`` scopes the sibling to a *linked* dataset by the name this catalog gives it
    (``DatasetLinks``, or ``bidslake link alias``), resolved in the **anchor's own**
    dataset. A hardcoded ``dataset_id`` cannot do this: ids are free text, and a study
    processed one subject at a time has one dataset per subject. Matching entities
    catalog-wide instead is unsound in a way the count below cannot catch (``docs/adr/0003``
    §7).

    Returns the sibling's ``dataset_id``/``root_uri``/``file_path`` (all three, because a
    path only means something together with the root it was walked from) and ``__n``, the
    match count::

        a = AllFiles.__table__.alias("a")
        cols, frm = [a.c.sub, a.c.ses, a.c.dataset_id, a.c.root_uri, a.c.file_path], a
        for role, (join, where) in ROLES.items():
            lat, sel = sibling(a, role, join, where, via="fmriprep")
            cols += sel
            frm = frm.outerjoin(lat, true())
        units = lake.sql(select(*cols).select_from(frm).where(a.c.suffix == "bold"))

    ``__n`` is the point, and it is a count rather than a raise: ``1`` resolved, ``0``
    missing — that subject is incomplete, which is data — and ``2+`` ambiguous, meaning
    ``join``/``where`` under-specify and the answer must not be silently taken. One query
    for the whole study, not one per role per unit; DuckDB decorrelates the laterals.
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
