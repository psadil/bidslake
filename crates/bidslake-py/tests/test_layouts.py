"""Layouts: building output paths for files that do not exist yet.

The read side (term maps) and the write side (layouts) describe one tree from opposite
directions. These pin the write side's behaviour; that the two *agree* is enforced when
the layout loads, and exercised here by round-tripping a rendered path back through an
actual index of a tree built from those very paths.
"""

from __future__ import annotations

import subprocess
import time
from pathlib import Path

import bidslake
import pytest

UNIT = "sub-01_ses-V1_task-rest_run-01_desc-preproc_bold"


@pytest.fixture(scope="module")
def feat() -> bidslake.Layout:
    return bidslake.layout("feat")


def test_roles_cover_the_feat_tree(feat: bidslake.Layout) -> None:
    # The slots a FIX/MELODIC run actually produces and downstream code reaches for.
    for role in (
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
    ):
        assert role in feat.roles
    assert feat.term_map == "feat"


def test_literal_roles_render_under_a_root(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT)
    assert at["filtered_func_clean"] == Path(f"/work/{UNIT}/filtered_func_data_clean.nii.gz")
    assert at["highres2standard_mat"] == Path(f"/work/{UNIT}/reg/highres2standard.mat")
    assert at["melodic_mix"] == Path(f"/work/{UNIT}/filtered_func_data.ica/melodic_mix")


def test_placeholders_come_from_keywords(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT)
    assert at.path("classification", training="UKBiobank", threshold="1") == Path(
        f"/work/{UNIT}/fix4melview_UKBiobank_thr1.txt"
    )
    assert at.path(
        "classification_by_rater", training="UKBiobank", threshold="1", rater="psadil"
    ) == Path(f"/work/{UNIT}/fix4melview_UKBiobank_thr1_psadil.txt")


def test_unbound_placeholder_raises_rather_than_guessing(feat: bidslake.Layout) -> None:
    """An empty substitution would produce a plausible path pointing at nothing."""
    at = feat.under(Path("/work") / UNIT)
    with pytest.raises(KeyError, match="unbound placeholders"):
        at["classification"]


def test_unknown_role_lists_what_exists(feat: bidslake.Layout) -> None:
    at = feat.under(Path("/work") / UNIT)
    with pytest.raises(KeyError, match="unknown role"):
        at["highres2standard"]  # the real role name ends in `_mat`


def test_unknown_layout_names_the_bundled_ones() -> None:
    with pytest.raises(RuntimeError, match="bundled layouts"):
        bidslake.layout("nope")


def test_mkdir_creates_the_parent(feat: bidslake.Layout, tmp_path: Path) -> None:
    target = feat.under(tmp_path / UNIT).mkdir("highres2standard_mat")
    assert target.parent.is_dir()
    assert not target.exists(), "only the directory is created, not the file"


def test_rendered_paths_index_back_as_declared(feat: bidslake.Layout, tmp_path: Path) -> None:
    """The end-to-end version of the round trip the layout checks at load time.

    Build a tree *from the layout's own rendered paths*, index it with the `feat`
    adapter, and confirm each file reads back as the role declared. This is what makes
    the two directions trustworthy rather than merely coexisting.
    """
    binary = (
        Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
        / "target"
        / "debug"
        / "bidslake"
    )
    if not binary.exists():
        pytest.skip("build the debug binary first: cargo build -p bidslake")

    root = tmp_path / "out"
    at = feat.under(root / UNIT)
    # `desc` is worth reading carefully. Where a role's mapping sets it, the projection
    # wins. Where it does not (`highres`, `wmparc` -- identified by `suffix` and `seg`
    # instead), the concept column falls back to the path regex, which finds
    # `desc-preproc` in the *unit directory* name. That is the input BOLD's desc, and
    # inheriting it is a true statement about which run the tree was built from.
    expected = {
        "filtered_func_clean": ("func", "bold", "clean"),
        "mask": ("func", "mask", "brain"),
        "melodic_mix": ("func", "mixing", "MELODIC"),
        "highres": ("anat", "T1w", "preproc"),
        "wmparc": ("anat", "dseg", "preproc"),
    }
    for role in expected:
        at.mkdir(role).write_bytes(b"x")

    db = tmp_path / "out.duckdb"
    subprocess.run(
        [
            str(binary),
            "index",
            "-i",
            str(root),
            "-o",
            str(db),
            "--dataset-id",
            "out",
            "--adapter",
            "feat",
        ],
        check=True,
        capture_output=True,
    )

    with bidslake.open(str(db)) as lake:
        rows = {
            Path(f.file_path).name: (f.datatype, f.suffix, f.entities.get("desc"))
            for f in lake.get()
        }
    for role, want in expected.items():
        name = at[role].name
        assert name in rows, f"{role} ({name}) was not indexed"
        assert rows[name] == want, f"{role} read back as {rows[name]}, declared {want}"


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
    # …and the binding is not mutated by having been overridden once.
    assert at["classification"] == Path(f"/work/{UNIT}/fix4melview_UKBiobank_thr1.txt")


def test_a_per_call_keyword_completes_a_partial_binding(feat: bidslake.Layout) -> None:
    """The `rater` case: run-wide values bound once, the per-file one supplied late."""
    at = feat.under(Path("/work") / UNIT, training="UKBiobank", threshold="1")
    assert at.path("classification_by_rater", rater="ab") == Path(
        f"/work/{UNIT}/fix4melview_UKBiobank_thr1_ab.txt"
    )


def test_every_role_renders_when_the_placeholders_are_bound(feat: bidslake.Layout) -> None:
    """The point of the change: walking `roles` needs no knowledge of which are special."""
    at = feat.under(Path("/work") / UNIT, **BINDINGS)
    rendered = [at[role] for role in feat.roles]
    assert len(rendered) == len(feat.roles)
    assert all(p.is_absolute() for p in rendered)


def test_binding_nothing_still_raises(feat: bidslake.Layout) -> None:
    """The guarantee ADR 0008 states is unchanged; only the moment of binding moved."""
    at = feat.under(Path("/work") / UNIT)
    with pytest.raises(KeyError, match="unbound placeholders"):
        at["classification"]


def test_a_mistyped_binding_says_what_is_bound(feat: bidslake.Layout) -> None:
    """Otherwise `under(root, trainng=…)` fails as 'unbound' to a caller who did bind it."""
    at = feat.under(Path("/work") / UNIT, trainng="UKBiobank", threshold="1")
    with pytest.raises(KeyError, match=r"bound here: threshold='1', trainng='UKBiobank'"):
        at["classification"]


def test_mkdir_inherits_the_bindings(feat: bidslake.Layout, tmp_path: Path) -> None:
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    target = at.mkdir("classification")
    assert target.parent.is_dir()
    assert target.name == "fix4melview_UKBiobank_thr1.txt"


def test_under_without_bindings_is_unchanged(feat: bidslake.Layout) -> None:
    """The 21 roles that need nothing keep working exactly as before."""
    at = feat.under(Path("/work") / UNIT)
    assert at["filtered_func"] == Path(f"/work/{UNIT}/filtered_func_data.nii.gz")
    assert at["highres2standard_mat"] == Path(f"/work/{UNIT}/reg/highres2standard.mat")


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


def test_presence_is_false_before_anything_is_written(feat, tmp_path):
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    assert at.has("filtered_func") is False
    assert set(at.present().values()) == {False}


def test_presence_follows_the_filesystem(feat, tmp_path):
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    at.mkdir("filtered_func").write_bytes(b"x" * 1234)
    assert at.has("filtered_func") is True
    assert at.present()["filtered_func"] is True
    assert at.present()["melodic_mix"] is False


def test_state_carries_size_and_mtime(feat, tmp_path):
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    at.mkdir("filtered_func").write_bytes(b"x" * 1234)
    st = at.state("filtered_func")
    assert (st.role, st.exists, st.size_bytes) == ("filtered_func", True, 1234)
    assert st.mtime_ns is not None
    absent = at.state("melodic_mix")
    assert (absent.exists, absent.size_bytes, absent.mtime_ns) == (False, None, None)


def test_an_unrenderable_role_is_omitted_not_reported_absent(feat, tmp_path):
    """Unaddressable and missing are different answers.

    Conflating them would report a finished tree as incomplete because a caller forgot a
    keyword — which is exactly the confusion the raise on `path()` exists to prevent.
    """
    at = feat.under(tmp_path / UNIT, training="UKBiobank", threshold="1")  # no `rater`
    present = at.present()
    assert "classification" in present, "bound placeholders make this one renderable"
    assert "classification_by_rater" not in present, "unbound `{rater}` — omitted, not False"
    # With `rater` supplied, it joins the rest.
    assert "classification_by_rater" in feat.under(tmp_path / UNIT, **BINDINGS).present()


def test_has_answers_false_rather_than_raising(feat, tmp_path):
    """`path()` raises for an unrenderable role; `has()` deliberately does not.

    A caller asking "is it there" wants an answer.
    """
    at = feat.under(tmp_path / UNIT, training="UKBiobank", threshold="1")
    with pytest.raises(KeyError):
        at["classification_by_rater"]
    assert at.has("classification_by_rater") is False


def test_digest_changes_when_a_file_appears(feat, tmp_path):
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    before = at.digest()
    at.mkdir("filtered_func").write_bytes(b"x" * 1234)
    assert at.digest() != before


def test_digest_changes_on_a_size_preserving_rewrite(feat, tmp_path):
    """The case a size-only check misses, and the reason mtime is in the digest."""
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    at.mkdir("filtered_func").write_bytes(b"x" * 1234)
    before = at.digest()
    time.sleep(0.01)
    at["filtered_func"].write_bytes(b"y" * 1234)  # same size, new mtime
    assert at.digest() != before


def test_digest_is_stable_when_nothing_moves(feat, tmp_path):
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    at.mkdir("filtered_func").write_bytes(b"x" * 1234)
    assert at.digest() == at.digest()


def test_digest_can_be_scoped_to_one_step_s_roles(feat, tmp_path):
    """What a per-step cache key needs: only the roles that step writes."""
    at = feat.under(tmp_path / UNIT, **BINDINGS)
    at.mkdir("filtered_func").write_bytes(b"x")
    scoped = at.digest("filtered_func")
    at.mkdir("melodic_mix").write_bytes(b"y")
    assert at.digest("filtered_func") == scoped, "an unrelated role must not move it"
    assert at.digest() != scoped, "but the whole-tree digest did move"
