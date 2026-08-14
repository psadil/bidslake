"""`sibling`: the one composition helper, and the three outcomes it distinguishes.

The fixture catalog is what makes these checks real rather than shaped to fit.
`eyetracking_fmri` is the unit-of-work case in miniature — two `task-rest` BOLD runs in
one session, sharing one T1w, each with a `.json` beside it carrying identical entities.
`ds210` is sessionless, so `ses` is NULL there and an `==` join would drop every row. And
every dataset is aliased to every dataset by name, so `via=` has both a right answer and a
wrong one available to it.
"""

from __future__ import annotations

import bidslake
from bidslake.schema.models import AllFiles
from sqlalchemy import select, true

#: The two BOLD runs of `eyetracking_fmri`, the anchor for most of these.
BOLD = {"dataset_id": "eyetracking_fmri", "suffix": "bold", "extension": ".nii.gz"}


def _units(lake, *roles, **anchor):
    """One row per anchor file, every role beside it.

    `root_uri` is selected alongside `dataset_id`/`file_path` so `sibling_path(row)` can
    read the anchor the same way it reads a sibling.
    """
    a = AllFiles.__table__.alias("a")
    cols = [a.c.dataset_id, a.c.root_uri, a.c.sub, a.c.ses, a.c.task, a.c.run, a.c.file_path]
    frm = a
    for name, join, where, via in roles:
        lat, sel = bidslake.sibling(a, name, join, where, via=via)
        cols += sel
        frm = frm.outerjoin(lat, true())
    return lake.sql(
        select(*cols)
        .select_from(frm)
        .where(a.c.kind == "data", *[a.c[k] == v for k, v in anchor.items()])
        .order_by(a.c.file_path)
    )


def test_a_sibling_matches_on_less_than_the_unit_key(lake) -> None:
    """The headline: two runs, one session, and the T1w they share.

    This is what `get()` cannot do — the anatomical is not found by the run's entities,
    it is found by a *subset* of them, once per run, in the same query.
    """
    df = _units(lake, ("anat", ("sub", "ses"), {"suffix": "T1w"}, None), **BOLD)
    assert df.height == 2, "fixture assumption: two BOLD runs in one session"
    assert df["anat__n"].to_list() == [1, 1]
    paths = df["anat__file_path"].to_list()
    assert paths[0] == paths[1] and paths[0].endswith("_T1w.nii.gz")
    # All three location columns, because a path only means something with its root.
    assert df["anat__dataset_id"].to_list() == ["eyetracking_fmri"] * 2
    assert all(r is not None for r in df["anat__root_uri"].to_list())


def test_a_missing_sibling_counts_zero_rather_than_dropping_the_unit(lake) -> None:
    """A per-unit gap is data. The anchor row survives with `__n == 0` and NULL paths."""
    df = _units(lake, ("nope", ("sub", "ses"), {"suffix": "T1w", "desc": "nosuch"}, None), **BOLD)
    assert df.height == 2, "the anchor rows must still be returned"
    assert df["nope__n"].to_list() == [0, 0]
    assert df["nope__file_path"].to_list() == [None, None]


def test_an_ambiguous_sibling_counts_above_one(lake) -> None:
    """Under-specify the join and the count says so, rather than one match being taken.

    Joining the *runs* on session alone is the mistake in miniature: each run matches
    both, and nothing about a single arbitrary answer would look wrong downstream.
    """
    df = _units(lake, ("run", ("sub", "ses"), {"suffix": "bold"}, None), **BOLD)
    assert df["run__n"].to_list() == [2, 2]
    # Adding the entity that separates them resolves it — same query, narrower join.
    fixed = _units(lake, ("run", ("sub", "ses", "run"), {"suffix": "bold"}, None), **BOLD)
    assert fixed["run__n"].to_list() == [1, 1]


def test_the_join_is_null_safe(lake) -> None:
    """`ds210` has no sessions. `ses = ses` is NULL for those rows, so `==` would drop
    every one of them — the failure mode that makes a sessionless dataset look empty."""
    df = _units(
        lake,
        ("mine", ("sub", "ses", "task", "run", "echo"), {"suffix": "bold"}, None),
        dataset_id="ds210",
        suffix="bold",
        extension=".nii.gz",
    )
    assert df.height > 0
    assert df["ses"].to_list() == [None] * df.height, "fixture assumption: sessionless"
    assert df["mine__n"].to_list() == [1] * df.height
    assert df["mine__file_path"].to_list() == df["file_path"].to_list()


def test_kind_data_is_the_default_and_is_overridable(lake) -> None:
    """Without it every sibling also matches its own JSON sidecar and reads as ambiguous.

    It is a default rather than a rule because the sidecar is sometimes the thing wanted.
    """
    join, where = ("sub", "ses", "task", "run"), {"suffix": "bold"}
    default = _units(lake, ("f", join, where, None), **BOLD)
    explicit = _units(lake, ("f", join, {**where, "kind": "sidecar"}, None), **BOLD)
    assert default["f__n"].to_list() == [1, 1]
    assert all(p.endswith(".nii.gz") for p in default["f__file_path"])
    assert explicit["f__n"].to_list() == [1, 1]
    assert all(p.endswith(".json") for p in explicit["f__file_path"])


def test_none_in_where_means_is_null(lake) -> None:
    """How a native-space image is separated from its `space-*` resamplings. Here the
    same shape: the session's T1w carries no `run`, and the BOLD beside it does."""
    hit = _units(lake, ("f", ("sub", "ses"), {"suffix": "T1w", "run": None}, None), **BOLD)
    miss = _units(lake, ("f", ("sub", "ses"), {"suffix": "T1w", "run": "01"}, None), **BOLD)
    assert hit["f__n"].to_list() == [1, 1]
    assert miss["f__n"].to_list() == [0, 0]


def test_via_scopes_the_sibling_to_the_linked_dataset(lake) -> None:
    """`via` is a link name resolved in the anchor's own dataset, not a `dataset_id`.

    The fixture aliases every dataset to every dataset, so the *only* thing selecting
    between them is the name — which is the property that makes a query portable across
    catalogs whose ids differ.
    """
    a = AllFiles.__table__.alias("a")
    cols = [a.c.dataset_id, a.c.file_path]
    frm = a
    for target in ("ds210", "eyetracking_fmri"):
        lat, sel = bidslake.sibling(
            a, target, (), {"suffix": "bold", "extension": ".nii.gz"}, via=target
        )
        cols += sel
        frm = frm.outerjoin(lat, true())
    df = lake.sql(
        select(*cols)
        .select_from(frm)
        .where(a.c.kind == "data", a.c.dataset_id == "ds001", a.c.suffix == "bold")
    )
    assert df.height > 0
    for target in ("ds210", "eyetracking_fmri"):
        ids = set(df[f"{target}__dataset_id"].to_list())
        assert ids == {target}, f"via={target} reached {ids}"


def test_via_finds_nothing_when_the_link_is_not_declared_here(lake) -> None:
    """A link declared in a *neighbouring* dataset is deliberately not in scope: the name
    is resolved in the anchor's dataset, which is what BIDS `DatasetLinks` means."""
    df = _units(lake, ("far", (), {"suffix": "bold"}, "nosuchlink"), **BOLD)
    assert df.height == 2
    assert df["far__n"].to_list() == [0, 0]


def test_many_siblings_are_still_one_query(lake) -> None:
    """The reason this is a lateral rather than a loop: cost scales with roles, not with
    roles x units. Eight of them still compile to a single statement."""
    roles = [(f"r{i}", ("sub", "ses"), {"suffix": "T1w"}, None) for i in range(8)]
    df = _units(lake, *roles, **BOLD)
    assert df.height == 2
    for i in range(8):
        assert df[f"r{i}__n"].to_list() == [1, 1]


# -- reading the columns back ------------------------------------------------
#
# `sibling()` invented the `<name>__dataset_id/__root_uri/__file_path/__n` convention.
# These two read it, so a consumer does not re-implement the unpack (which is what both
# a2cps pipelines had done, identically).


def test_sibling_path_resolves_a_role_and_the_anchor(lake) -> None:
    """The three location columns are one path, and `name=None` means the anchor."""
    df = _units(lake, ("anat", ("sub", "ses"), {"suffix": "T1w"}, None), **BOLD)
    row = df.row(0, named=True)

    anat = bidslake.sibling_path(lake, row, "anat")
    assert anat.name.endswith("_T1w.nii.gz")
    assert bidslake.to_local_path(anat).is_file(), "must resolve to the real file on disk"

    anchor = bidslake.sibling_path(lake, row)
    assert anchor.name.endswith("_bold.nii.gz")
    assert str(anchor).endswith(row["file_path"])

    # Same answer as the long form it replaces.
    assert anat == lake.resolve(
        row["anat__dataset_id"], row["anat__file_path"], row["anat__root_uri"]
    )


def test_to_local_path_takes_a_upath_as_well_as_a_str(lake) -> None:
    """So a call site is not `to_local_path(str(lake.resolve(...)))`."""
    df = _units(lake, ("anat", ("sub", "ses"), {"suffix": "T1w"}, None), **BOLD)
    p = bidslake.sibling_path(lake, df.row(0, named=True), "anat")
    assert bidslake.to_local_path(p) == bidslake.to_local_path(str(p))


def test_unresolved_reports_only_the_siblings_that_are_not_exactly_one(lake) -> None:
    """Empty when every role resolved; otherwise the count, which separates the two
    failures — 0 is a subject to skip, 2+ is a query to fix."""
    roles = (
        ("anat", ("sub", "ses"), {"suffix": "T1w"}, None),  # resolves
        ("nope", ("sub", "ses"), {"suffix": "T1w", "desc": "x"}, None),  # missing
        ("both", ("sub", "ses"), {"suffix": "bold"}, None),  # both runs -> 2
    )
    row = _units(lake, *roles, **BOLD).row(0, named=True)

    assert bidslake.unresolved(row, ["anat"]) == {}
    assert bidslake.unresolved(row, ["nope"]) == {"nope": 0}
    assert bidslake.unresolved(row, ["both"]) == {"both": 2}
    assert bidslake.unresolved(row, [n for n, *_ in roles]) == {"nope": 0, "both": 2}


def test_unresolved_accepts_any_iterable_of_names(lake) -> None:
    """A dict of roles is the natural thing to pass, and iterating it yields its keys."""
    row = _units(lake, ("anat", ("sub", "ses"), {"suffix": "T1w"}, None), **BOLD).row(0, named=True)
    assert bidslake.unresolved(row, {"anat": "..."}) == {}
