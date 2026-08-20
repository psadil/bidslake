"""`sibling`: the one composition helper, and the three outcomes it distinguishes.

The fixture catalog is what makes these checks real rather than shaped to fit.
`eyetracking_fmri` is the unit-of-work case in miniature — two `task-rest` BOLD runs in
one session, sharing one T1w, each with a `.json` beside it carrying identical entities.
`ds210` is sessionless, so `ses` is NULL there and an `==` join would drop every row. And
every dataset is aliased to every dataset by name, so `via=` has both a right answer and a
wrong one available to it.

`_units()` is the Act throughout, so each family of checks shares one fixture holding its
result rather than re-running the query per assertion.
"""

from __future__ import annotations

import bidslake
import pytest
from bidslake.schema.models import AllFiles
from sqlalchemy import select, true

#: The two BOLD runs of `eyetracking_fmri`, the anchor for most of these.
BOLD = {"dataset_id": "eyetracking_fmri", "suffix": "bold", "extension": ".nii.gz"}

#: The anatomical every run in the session shares, found by a *subset* of the run's key.
ANAT = ("anat", ("sub", "ses"), {"suffix": "T1w", "extension": ".nii.gz"}, None)


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
        .where(a.c.extension == ".nii.gz", *[a.c[k] == v for k, v in anchor.items()])
        .order_by(a.c.file_path)
    )


@pytest.fixture(scope="module")
def bold_with_anat(lake):
    """The headline: two runs, one session, and the T1w they share."""
    df = _units(lake, ANAT, **BOLD)
    assert df.height == 2, "fixture assumption: two BOLD runs in one session"
    return df


def test_a_sibling_matches_on_less_than_the_unit_key(bold_with_anat):
    """What `get()` cannot do.

    The anatomical is not found by the run's entities, it is found by a *subset* of them,
    once per run, in the same query.
    """
    assert bold_with_anat["anat__n"].to_list() == [1, 1]


def test_both_runs_resolve_to_the_same_anatomical(bold_with_anat):
    paths = bold_with_anat["anat__file_path"].to_list()

    assert len(set(paths)) == 1


def test_the_matched_sibling_is_the_anatomical(bold_with_anat):
    paths = bold_with_anat["anat__file_path"].to_list()

    assert {p.endswith("_T1w.nii.gz") for p in paths} == {True}


def test_a_sibling_carries_its_dataset(bold_with_anat):
    """All three location columns, because a path only means something with its root."""
    assert bold_with_anat["anat__dataset_id"].to_list() == ["eyetracking_fmri"] * 2


def test_a_sibling_carries_its_root(bold_with_anat):
    assert None not in bold_with_anat["anat__root_uri"].to_list()


# -- the two failures a count distinguishes ----------------------------------


@pytest.fixture(scope="module")
def bold_with_missing_sibling(lake):
    df = _units(lake, ("nope", ("sub", "ses"), {"suffix": "T1w", "desc": "nosuch"}, None), **BOLD)
    assert df.height == 2, "the anchor rows must still be returned"
    return df


def test_a_missing_sibling_counts_zero(bold_with_missing_sibling):
    """A per-unit gap is data. The anchor row survives rather than being dropped."""
    assert bold_with_missing_sibling["nope__n"].to_list() == [0, 0]


def test_a_missing_sibling_has_a_null_path(bold_with_missing_sibling):
    assert bold_with_missing_sibling["nope__file_path"].to_list() == [None, None]


def test_an_ambiguous_sibling_counts_above_one(lake):
    """Under-specify the join and the count says so, rather than one match being taken.

    Joining the *runs* on session alone is the mistake in miniature: each run matches
    both, and nothing about a single arbitrary answer would look wrong downstream.
    """
    df = _units(
        lake, ("run", ("sub", "ses"), {"suffix": "bold", "extension": ".nii.gz"}, None), **BOLD
    )

    assert df["run__n"].to_list() == [2, 2]


def test_narrowing_the_join_resolves_the_ambiguity(lake):
    """Same query, one more entity in the join — the one that separates the runs."""
    df = _units(
        lake,
        ("run", ("sub", "ses", "run"), {"suffix": "bold", "extension": ".nii.gz"}, None),
        **BOLD,
    )

    assert df["run__n"].to_list() == [1, 1]


# -- NULL-safety, which a sessionless dataset is the test case for -----------


@pytest.fixture(scope="module")
def sessionless_units(lake):
    """`ds210` has no sessions, so `ses = ses` is NULL for every one of its rows."""
    df = _units(
        lake,
        ("mine", ("sub", "ses", "task", "run", "echo"), {"suffix": "bold"}, None),
        dataset_id="ds210",
        suffix="bold",
        extension=".nii.gz",
    )
    assert df.height > 0, "fixture assumption: ds210 has BOLD images"
    assert df["ses"].to_list() == [None] * df.height, "fixture assumption: sessionless"
    return df


def test_the_join_is_null_safe(sessionless_units):
    """`==` would drop every row — the failure mode that makes a sessionless dataset empty."""
    height = sessionless_units.height

    assert sessionless_units["mine__n"].to_list() == [1] * height


def test_a_null_safe_join_matches_the_right_row(sessionless_units):
    """Joined on its whole key, each run's only match is itself."""
    assert (
        sessionless_units["mine__file_path"].to_list() == sessionless_units["file_path"].to_list()
    )


# -- the `where` discriminator -----------------------------------------------


#: The same sibling query with `extension` pinned each way and left unpinned: the match
#: count per unit, and the extension a resolved match must carry.
EXTENSION_CASES = {
    "image": ({"suffix": "bold", "extension": ".nii.gz"}, [1, 1], ".nii.gz"),
    "sidecar": ({"suffix": "bold", "extension": ".json"}, [1, 1], ".json"),
    "unpinned": ({"suffix": "bold"}, [2, 2], None),
}


@pytest.fixture(scope="module", params=list(EXTENSION_CASES))
def sibling_by_extension(request: pytest.FixtureRequest, lake):
    where, counts, extension = EXTENSION_CASES[request.param]
    df = _units(lake, ("f", ("sub", "ses", "task", "run"), where, None), **BOLD)
    return df, counts, extension


def test_an_unpinned_role_reads_as_ambiguous(sibling_by_extension):
    """An undiscriminating role matches the image and its sidecar, and says so.

    `all_files` is the whole registry: an image and its `.json` sidecar share every
    entity, so a role whose `where` pins no discriminating column matches both and reads
    as ambiguous (`__n == 2`) rather than silently picking one.
    """
    df, counts, _ = sibling_by_extension

    assert df["f__n"].to_list() == counts


def test_the_match_is_what_the_extension_pin_says(sibling_by_extension):
    """Counting one is not enough on its own.

    It would also count one if the image pin had matched the sidecar and vice versa.
    """
    df, _, extension = sibling_by_extension
    if extension is None:
        pytest.skip("the unpinned case resolves nothing to check")

    assert {p.endswith(extension) for p in df["f__file_path"]} == {True}


@pytest.mark.parametrize(("run", "expected"), [(None, [1, 1]), ("01", [0, 0])], ids=["null", "set"])
def test_none_in_where_means_is_null(lake, run, expected):
    """How a native-space image is separated from its `space-*` resamplings.

    Here the same shape: the session's T1w carries no `run`, and the BOLD beside it does.
    """
    df = _units(
        lake,
        ("f", ("sub", "ses"), {"suffix": "T1w", "extension": ".nii.gz", "run": run}, None),
        **BOLD,
    )

    assert df["f__n"].to_list() == expected


# -- `via`, which resolves a link name rather than a dataset id --------------

VIA_TARGETS = ("ds210", "eyetracking_fmri")


@pytest.fixture(scope="module")
def via_scoped(lake):
    """One ds001 anchor per row, with a BOLD sibling reached through each link name."""
    a = AllFiles.__table__.alias("a")
    cols = [a.c.dataset_id, a.c.file_path]
    frm = a
    for target in VIA_TARGETS:
        lat, sel = bidslake.sibling(
            a, target, (), {"suffix": "bold", "extension": ".nii.gz"}, via=target
        )
        cols += sel
        frm = frm.outerjoin(lat, true())
    df = lake.sql(
        select(*cols)
        .select_from(frm)
        .where(a.c.extension == ".nii.gz", a.c.dataset_id == "ds001", a.c.suffix == "bold")
    )
    assert df.height > 0, "fixture assumption: ds001 has BOLD data files"
    return df


@pytest.mark.parametrize("target", VIA_TARGETS)
def test_via_scopes_the_sibling_to_the_linked_dataset(via_scoped, target):
    """`via` is a link name resolved in the anchor's own dataset, not a `dataset_id`.

    The fixture aliases every dataset to every dataset, so the *only* thing selecting
    between them is the name — which is the property that makes a query portable across
    catalogs whose ids differ.
    """
    reached = set(via_scoped[f"{target}__dataset_id"].to_list())

    assert reached == {target}


@pytest.fixture(scope="module")
def via_undeclared(lake):
    df = _units(lake, ("far", (), {"suffix": "bold"}, "nosuchlink"), **BOLD)
    assert df.height == 2, "the anchor rows must still be returned"
    return df


def test_via_finds_nothing_when_the_link_is_not_declared_here(via_undeclared):
    """A link declared in a *neighbouring* dataset is deliberately not in scope.

    The name is resolved in the anchor's dataset, which is what BIDS `DatasetLinks` means.
    """
    assert via_undeclared["far__n"].to_list() == [0, 0]


def test_many_siblings_are_still_one_query(lake):
    """The reason this is a lateral rather than a loop.

    Cost scales with roles, not with roles x units. Eight of them still compile to a single
    statement.
    """
    roles = [
        (f"r{i}", ("sub", "ses"), {"suffix": "T1w", "extension": ".nii.gz"}, None) for i in range(8)
    ]

    df = _units(lake, *roles, **BOLD)

    counts = {f"r{i}": df[f"r{i}__n"].to_list() for i in range(8)}
    assert counts == {f"r{i}": [1, 1] for i in range(8)}


# -- reading the columns back ------------------------------------------------
#
# `sibling()` invented the `<name>__dataset_id/__root_uri/__file_path/__n` convention.
# These read it, so a consumer does not re-implement the unpack (which is what both
# a2cps pipelines had done, identically).


@pytest.fixture(scope="module")
def unit_row(bold_with_anat):
    """One anchor row, with its `anat` role beside it."""
    return bold_with_anat.row(0, named=True)


def test_sibling_path_resolves_a_role(lake, unit_row):
    """The three location columns are one path."""
    anat = bidslake.sibling_path(lake, unit_row, "anat")

    assert anat.name.endswith("_T1w.nii.gz")


def test_a_resolved_sibling_is_the_real_file_on_disk(lake, unit_row):
    anat = bidslake.sibling_path(lake, unit_row, "anat")

    assert bidslake.to_local_path(anat).is_file()


def test_sibling_path_resolves_the_anchor_when_name_is_none(lake, unit_row):
    """`name=None` means the anchor, read exactly the way a sibling is."""
    anchor = bidslake.sibling_path(lake, unit_row)

    assert anchor.name.endswith("_bold.nii.gz") and str(anchor).endswith(unit_row["file_path"])


def test_sibling_path_agrees_with_the_long_form_it_replaces(lake, unit_row):
    """The pair of calls is the claim: `sibling_path` is `resolve` over the convention."""
    short = bidslake.sibling_path(lake, unit_row, "anat")
    long = lake.resolve(
        unit_row["anat__dataset_id"], unit_row["anat__file_path"], unit_row["anat__root_uri"]
    )

    assert short == long


def test_to_local_path_takes_a_upath_as_well_as_a_str(lake, unit_row):
    """So a call site is not `to_local_path(str(lake.resolve(...)))`.

    That the two spellings agree is the claim, so the pair of calls is one Act.
    """
    p = bidslake.sibling_path(lake, unit_row, "anat")

    assert bidslake.to_local_path(p) == bidslake.to_local_path(str(p))


# -- `unresolved`, which separates the two failures --------------------------

ROLES = (
    ANAT,  # resolves
    ("nope", ("sub", "ses"), {"suffix": "T1w", "desc": "x"}, None),  # missing
    ("both", ("sub", "ses"), {"suffix": "bold", "extension": ".nii.gz"}, None),  # both runs -> 2
)


@pytest.fixture(scope="module")
def mixed_row(lake):
    """One row carrying a role of each outcome: exactly one, none, and two."""
    return _units(lake, *ROLES, **BOLD).row(0, named=True)


@pytest.mark.parametrize(
    ("names", "expected"),
    [
        (["anat"], {}),
        (["nope"], {"nope": 0}),
        (["both"], {"both": 2}),
        ([name for name, *_ in ROLES], {"nope": 0, "both": 2}),
    ],
    ids=["resolved", "missing", "ambiguous", "all"],
)
def test_unresolved_reports_only_the_siblings_that_are_not_exactly_one(mixed_row, names, expected):
    """Empty when every role resolved; otherwise the count.

    The count separates the two failures — 0 is a subject to skip, 2+ is a query to fix.
    """
    assert bidslake.unresolved(mixed_row, names) == expected


def test_unresolved_accepts_any_iterable_of_names(mixed_row):
    """A dict of roles is the natural thing to pass, and iterating it yields its keys."""
    assert bidslake.unresolved(mixed_row, {"anat": "..."}) == {}
