"""Layouts: building output paths for files that do not exist yet.

The read side (term maps) and the write side (layouts) describe one tree from opposite
directions. These pin the write side's behaviour; that the two *agree* is enforced when
the layout loads, and exercised here by round-tripping a rendered path back through an
actual index of a tree built from those very paths.
"""

from __future__ import annotations

import os
import time
from collections.abc import Callable
from pathlib import Path

import bidslake
import pytest

UNIT = "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold"

#: The slots a FIX/MELODIC run actually produces and downstream code reaches for.
FEAT_ROLES = (
    "filtered_func",
    "filtered_func_clean",
    "mask",
    "melodic_mix",
    "motion_params",
    "classification",
    "example_func",
    "highres",
    "standard",
    "wmparc",
    "highres2standard_mat",
    "standard2example_func_warp",
)


@pytest.fixture(scope="module")
def feat() -> bidslake.Layout:
    return bidslake.layout("feat")


@pytest.mark.parametrize("role", FEAT_ROLES)
def test_roles_cover_the_feat_tree(feat: bidslake.Layout, role: str) -> None:
    assert role in feat.roles


def test_the_layout_names_its_term_map(feat: bidslake.Layout) -> None:
    assert feat.term_map == "feat"


# -- rendering ---------------------------------------------------------------
#
# The roles needing no bindings are rendered from `under()` alone; this is also what pins
# that the 21 such roles keep working when nothing is bound.


@pytest.mark.parametrize(
    ("role", "relative"),
    [
        ("filtered_func", "filtered_func_data.nii.gz"),
        ("filtered_func_clean", "filtered_func_data_clean.nii.gz"),
        ("highres2standard_mat", "reg/highres2standard.mat"),
        ("melodic_mix", "filtered_func_data.ica/melodic_mix"),
    ],
)
def test_literal_roles_render_under_a_root(feat: bidslake.Layout, role: str, relative: str) -> None:
    at = feat.under(Path("/work") / UNIT)

    assert at[role] == Path(f"/work/{UNIT}/{relative}")


@pytest.mark.parametrize(
    ("role", "keywords", "relative"),
    [
        (
            "classification",
            {"training": "UKBiobank", "threshold": "1"},
            "fix4melview_UKBiobank_thr1.txt",
        ),
        (
            "classification_by_rater",
            {"training": "UKBiobank", "threshold": "1", "rater": "psadil"},
            "fix4melview_UKBiobank_thr1_psadil.txt",
        ),
    ],
)
def test_placeholders_come_from_keywords(
    feat: bidslake.Layout, role: str, keywords: dict[str, str], relative: str
) -> None:
    at = feat.under(Path("/work") / UNIT)

    assert at.path(role, **keywords) == Path(f"/work/{UNIT}/{relative}")


def test_unbound_placeholder_raises_rather_than_guessing(feat: bidslake.Layout) -> None:
    """An empty substitution would produce a plausible path pointing at nothing.

    The guarantee ADR 0008 states; binding at `under()` (below) moved only the *moment*
    of binding, not this.
    """
    at = feat.under(Path("/work") / UNIT)

    with pytest.raises(KeyError, match="is unbound"):
        at["classification"]


def test_unknown_role_lists_what_exists(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT)

    with pytest.raises(KeyError, match="unknown role"):
        at["highres2standard"]  # the real role name ends in `_mat`


def test_unknown_layout_names_the_bundled_ones() -> None:
    with pytest.raises(RuntimeError, match="bundled layouts"):
        bidslake.layout("nope")


@pytest.fixture
def mkdir_target(feat: bidslake.Layout, tmp_path: Path) -> Path:
    return feat.under(tmp_path / UNIT).mkdir("highres2standard_mat")


def test_mkdir_creates_the_parent(mkdir_target: Path) -> None:
    assert mkdir_target.parent.is_dir()


def test_mkdir_does_not_create_the_file(mkdir_target: Path) -> None:
    """Only the directory is created; the file is the caller's to write."""
    assert not mkdir_target.exists()


# -- the round trip ----------------------------------------------------------
#
# `desc` is worth reading carefully. Where a role's mapping sets it, the projection wins.
# Where it does not (`highres`, `wmparc` -- identified by `suffix` and `seg` instead), the
# concept column falls back to the path regex, which finds `desc-preproc` in the *unit
# directory* name. That is the input BOLD's desc, and inheriting it is a true statement
# about which run the tree was built from.

ROUND_TRIP = {
    "filtered_func_clean": ("func", "bold", "clean"),
    "mask": ("func", "mask", "brain"),
    "melodic_mix": ("func", "mixing", "MELODIC"),
    "highres": ("anat", "T1w", "preproc"),
    "wmparc": ("anat", "dseg", "preproc"),
}


@pytest.fixture(scope="module")
def indexed_feat_tree(
    feat: bidslake.Layout,
    tmp_path_factory: pytest.TempPathFactory,
    bidslake_cli: Callable[..., None],
):
    """A tree built *from the layout's own rendered paths*, indexed with the `feat` adapter.

    The end-to-end version of the round trip the layout checks at load time, and what makes
    the two directions trustworthy rather than merely coexisting.
    """
    base = tmp_path_factory.mktemp("roundtrip")
    root = base / "out"
    at = feat.under(root / UNIT)
    for role in ROUND_TRIP:
        at.mkdir(role).write_bytes(b"x")

    db = base / "out.duckdb"
    bidslake_cli("index", "-i", root, "-o", db, "--dataset-id", "out", "--adapter", "feat")

    with bidslake.open(str(db)) as lake:
        rows = {
            Path(f.file_path).name: (f.datatype, f.suffix, f.entities.get("desc"))
            for f in lake.get()
        }
    return at, rows


@pytest.mark.parametrize(("role", "expected"), sorted(ROUND_TRIP.items()))
def test_rendered_paths_index_back_as_declared(
    indexed_feat_tree, role: str, expected: tuple[str, str, str]
) -> None:
    at, rows = indexed_feat_tree
    name = at[role].name

    # `.get`, so a role that was never indexed at all reads as `None` here rather than
    # raising something unrelated to what is being checked.
    assert rows.get(name) == expected


# ---------------------------------------------------------------------------
# Placeholders bound once, at `under()`
#
# `feat`'s two `classification` roles render `fix4melview_{training}_thr{threshold}.txt`,
# and those values are the pipeline's configuration rather than anything the layout knows.
# Raising for an unbound one is right (docs/adr/0008) and is pinned above. What these add is
# that the values may be supplied *once*, where the root is, since they have the same
# lifetime — before this, `for role in layout.roles: at[role]` was an error and every
# consumer that walked the role list special-cased the same two names by hand.
# ---------------------------------------------------------------------------

BINDINGS = {"training": "UKBiobank", "threshold": "1", "rater": "psadil"}


def test_bindings_given_to_under_are_inherited(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT, training="UKBiobank", threshold="1")

    assert at["classification"] == Path(f"/work/{UNIT}/fix4melview_UKBiobank_thr1.txt")


def test_a_per_call_keyword_overrides_a_bound_one(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT, **BINDINGS)

    assert at.path("classification", threshold="20") == Path(
        f"/work/{UNIT}/fix4melview_UKBiobank_thr20.txt"
    )


def test_an_override_does_not_mutate_the_binding(feat: bidslake.Layout) -> None:
    """The override is per call, so the next lookup sees the run-wide value again."""
    at = feat.under(Path("/work") / UNIT, **BINDINGS)
    at.path("classification", threshold="20")

    assert at["classification"] == Path(f"/work/{UNIT}/fix4melview_UKBiobank_thr1.txt")


def test_a_per_call_keyword_completes_a_partial_binding(feat: bidslake.Layout) -> None:
    """The `rater` case: run-wide values bound once, the per-file one supplied late."""
    at = feat.under(Path("/work") / UNIT, training="UKBiobank", threshold="1")

    assert at.path("classification_by_rater", rater="ab") == Path(
        f"/work/{UNIT}/fix4melview_UKBiobank_thr1_ab.txt"
    )


def test_every_role_renders_when_the_placeholders_are_bound(feat: bidslake.Layout) -> None:
    """The point of the change: walking `roles` needs no knowledge of which are special.

    That every role renders is the comprehension itself — an unbound one would raise. What
    is left to assert is that each rendered path is usable.
    """
    at = feat.under(Path("/work") / UNIT, **BINDINGS)

    rendered = [at[role] for role in feat.roles]

    assert {p.is_absolute() for p in rendered} == {True}


def test_a_mistyped_binding_says_what_is_bound(feat: bidslake.Layout) -> None:
    """Otherwise `under(root, trainng=…)` fails as 'unbound' to a caller who did bind it."""
    at = feat.under(Path("/work") / UNIT, trainng="UKBiobank", threshold="1")

    with pytest.raises(KeyError, match=r"bound here: threshold='1', trainng='UKBiobank'"):
        at["classification"]


@pytest.fixture
def bound_mkdir_target(feat: bidslake.Layout, tmp_path: Path) -> Path:
    return feat.under(tmp_path / UNIT, **BINDINGS).mkdir("classification")


def test_mkdir_creates_the_parent_of_a_bound_role(bound_mkdir_target: Path) -> None:
    assert bound_mkdir_target.parent.is_dir()


def test_mkdir_inherits_the_bindings(bound_mkdir_target: Path) -> None:
    assert bound_mkdir_target.name == "fix4melview_UKBiobank_thr1.txt"


# ---------------------------------------------------------------------------
# Presence and identity
#
# "Is this role's file there, and has it changed" is the question every consumer that
# tracks progress through a tree has to answer, and before these it was answered four
# separate times in one spike: a pipeline's own done-check, a workflow engine's completion
# rule, an asset check comparing a ledger against disk, and a progress count for a UI.
#
# The work is in Rust so this and `bidslake verify` share one meaning of "changed" — the
# same size/mtime pair the ingest records in `all_files`.
# ---------------------------------------------------------------------------


@pytest.fixture
def empty_tree(feat, tmp_path):
    """A bound layout over a root where nothing has been written yet."""
    return feat.under(tmp_path / UNIT, **BINDINGS)


def test_has_is_false_before_anything_is_written(empty_tree):
    assert empty_tree.has("filtered_func") is False


def test_no_role_is_present_before_anything_is_written(empty_tree):
    assert set(empty_tree.present().values()) == {False}


@pytest.fixture
def one_written_role(feat, tmp_path):
    """The same, with exactly one role's file written."""
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    at.mkdir("filtered_func").write_bytes(b"x" * 1234)
    return at


def test_has_follows_the_filesystem(one_written_role):
    assert one_written_role.has("filtered_func") is True


def test_present_reports_the_written_role(one_written_role):
    assert one_written_role.present()["filtered_func"] is True


def test_present_still_reports_the_unwritten_ones(one_written_role):
    assert one_written_role.present()["melodic_mix"] is False


def test_state_carries_size_and_mtime(one_written_role):
    state = one_written_role.state("filtered_func")

    assert (state.role, state.exists, state.size_bytes) == ("filtered_func", True, 1234)


def test_a_present_role_has_an_mtime(one_written_role):
    assert one_written_role.state("filtered_func").mtime_ns is not None


def test_an_absent_role_has_no_stat(one_written_role):
    state = one_written_role.state("melodic_mix")

    assert (state.exists, state.size_bytes, state.mtime_ns) == (False, None, None)


# -- unaddressable is not the same answer as missing -------------------------


@pytest.fixture
def partially_bound(feat, tmp_path):
    """`training`/`threshold` bound, `rater` deliberately not."""
    return feat.under(tmp_path / UNIT, training="UKBiobank", threshold="1")


def test_a_renderable_role_is_reported(partially_bound):
    assert "classification" in partially_bound.present()


def test_an_unrenderable_role_is_omitted_not_reported_absent(partially_bound):
    """Conflating them would report a finished tree as incomplete because a caller forgot a
    keyword — which is exactly the confusion the raise on `path()` exists to prevent."""
    assert "classification_by_rater" not in partially_bound.present()


def test_supplying_the_last_keyword_makes_it_renderable(feat, tmp_path):
    at = feat.under(tmp_path / UNIT, **BINDINGS)

    assert "classification_by_rater" in at.present()


def test_has_answers_false_rather_than_raising(partially_bound):
    """`path()` raises for an unrenderable role; `has()` deliberately does not.

    A caller asking "is it there" wants an answer. That `path()` raises is pinned by
    `test_unbound_placeholder_raises_rather_than_guessing`.
    """
    assert partially_bound.has("classification_by_rater") is False


# -- the digest, which is a cache key ----------------------------------------


def test_digest_changes_when_a_file_appears(empty_tree):
    before = empty_tree.digest()

    empty_tree.mkdir("filtered_func").write_bytes(b"x" * 1234)

    assert empty_tree.digest() != before


def test_digest_changes_on_a_size_preserving_rewrite(one_written_role):
    """The case a size-only check misses, and the reason mtime is in the digest."""
    before = one_written_role.digest()
    time.sleep(0.01)

    one_written_role["filtered_func"].write_bytes(b"y" * 1234)  # same size, new mtime

    assert one_written_role.digest() != before


def test_digest_is_stable_when_nothing_moves(one_written_role):
    assert one_written_role.digest() == one_written_role.digest()


@pytest.fixture
def after_an_unrelated_write(one_written_role):
    """The digests taken before an *unrelated* role was written, and the tree after."""
    scoped = one_written_role.digest("filtered_func")
    whole = one_written_role.digest()
    one_written_role.mkdir("melodic_mix").write_bytes(b"y")
    return one_written_role, scoped, whole


def test_a_scoped_digest_ignores_an_unrelated_role(after_an_unrelated_write):
    """What a per-step cache key needs: only the roles that step writes."""
    at, scoped, _ = after_an_unrelated_write

    assert at.digest("filtered_func") == scoped


def test_the_whole_tree_digest_still_moves(after_an_unrelated_write):
    at, _, whole = after_an_unrelated_write

    assert at.digest() != whole


#: Binding values that put a real `..` *segment* into the rendered path, so it normalizes
#: outside the root it was joined to. `feat`'s placeholder sits mid-segment
#: (`fix4melview_{training}_thr{threshold}.txt`), so a value has to carry its own separators
#: to produce one — which is exactly why the check is on the rendered path rather than on the
#: binding.
TRAVERSING_BINDINGS = ["a/../../../etc/cron.d/x", "x/../..", "../../y"]


@pytest.mark.parametrize("training", TRAVERSING_BINDINGS, ids=lambda v: v)
def test_a_binding_cannot_render_a_path_outside_the_root(tmp_path, training):
    """A binding that would traverse out of the root is refused rather than rendered.

    ``check_template`` guarded the *template*, which the layout document controls, while
    binding values were substituted verbatim — so a caller reconstituted exactly the shape it
    exists to refuse. ``training='a/../../../etc/cron.d/x'`` rendered a path whose
    ``normpath`` is ``/etc/cron.d/x_thr20.txt``, and :meth:`mkdir` calls
    ``mkdir(parents=True)`` on whatever comes back.

    The naive check does not catch it: the rendered string *does* start with the root.
    """
    at = bidslake.layout("feat").under(tmp_path)

    with pytest.raises(KeyError, match="could not be rendered"):
        at.path("classification", training=training, threshold="20")


#: Values that *look* dangerous and are not, because the placeholder is mid-segment: each
#: renders one literal filename. Kept as a test because the tempting fix — rejecting any
#: binding containing `..` or `/` — would break all three, and they are legal.
INNOCENT_BINDINGS = ["..", "../..", "/etc/passwd"]


@pytest.mark.parametrize("training", INNOCENT_BINDINGS, ids=lambda v: v)
def test_a_binding_that_stays_inside_the_root_still_renders(tmp_path, training):
    """`..` inside a filename is not a traversal, and is not refused for looking like one.

    ``training='..'`` renders ``fix4melview_.._thr20.txt`` — one component, no parent
    reference. The guard is on the rendered path's *segments*, so it declines exactly the
    values that produce a `..` segment and no others.
    """
    at = bidslake.layout("feat").under(tmp_path)

    rendered = at.path("classification", training=training, threshold="20")

    assert Path(os.path.normpath(rendered)).is_relative_to(tmp_path)
