"""Bindings: units of work and their inputs, resolved from the catalog.

The fixture catalog is the interesting shape for this: ``ds001`` is sessionless with
one T1w and three BOLD runs per subject (so a sibling matched on ``sub`` alone has a
different join arity from the anchor's key), ``ds210`` has BOLD but no anat at all (so
an input can be genuinely missing), and both carry ``ses IS NULL`` throughout.
"""

from __future__ import annotations

import pytest
from bidslake import Binding, FileInput, TableInput

BOLD = {"datatype": "func", "suffix": "bold", "extension": ".nii.gz"}


ANAT = {"datatype": "anat", "suffix": "T1w", "extension": ".nii.gz"}


def _ds001(**extra):
    return Binding(
        anchor={**BOLD, "dataset_id": "ds001", **extra},
        key=("sub", "ses", "task", "run"),
        inputs={
            # One T1w per subject, so this joins on `sub` alone -- a strict subset of
            # the key, which is the whole reason a binding is not just a filter.
            # Scoped to the dataset because subject labels are not unique across a
            # pool of datasets (see test_dataset_id_scopes_an_input).
            "anat": FileInput(join=("sub",), dataset_id="ds001", where=ANAT),
        },
    )


def test_binds_units_with_a_narrower_join(lake) -> None:
    units = list(lake.bind(_ds001()))
    assert len(units) == 48, "3 runs x 16 subjects"
    assert all(not u.unresolved for u in units)

    # Every run of a subject resolves to that subject's single T1w.
    by_sub: dict[str, set[str]] = {}
    for u in units:
        by_sub.setdefault(u.entities["sub"], set()).add(str(u.inputs["anat"]))
    assert len(by_sub) == 16
    assert all(len(paths) == 1 for paths in by_sub.values())
    for sub, paths in by_sub.items():
        assert f"sub-{sub}" in next(iter(paths))


def test_key_and_anchor_are_carried(lake) -> None:
    unit = next(iter(lake.bind(_ds001(sub="01", run="01"))))
    assert unit.key == ("01", None, "balloonanalogrisktask", "01")
    # The anchor is the full BidsFile, so its path and merged sidecar come along --
    # no second lookup to get at the metadata the catalog already holds.
    assert unit.anchor.sub == "01"
    assert unit.anchor.local_path.name.endswith("_bold.nii.gz")
    assert unit.anchor.metadata["RepetitionTime"] > 0


def test_sessionless_units_join_on_null(lake) -> None:
    """`ds001` has no sessions, so both sides carry ``ses = NULL``.

    A SQL join would drop these (NULL never equals NULL); matching on the tuple of
    join values is what makes a sessionless dataset behave like any other.
    """
    units = list(lake.bind(_ds001(sub="01")))
    assert units and all(u.key[1] is None for u in units)
    assert all("anat" in u.inputs for u in units)


def test_missing_input_is_data_not_an_exception(lake) -> None:
    """ds210 has BOLD but ships no anat, so the input cannot resolve.

    The point is that iteration completes and every unit is inspectable: a subject
    missing an input is visible before any work is submitted, rather than raising
    partway through a long run.
    """
    binding = Binding(
        anchor={**BOLD, "dataset_id": "ds210", "task": "rest"},
        key=("sub", "ses", "task", "run"),
        inputs={
            "anat": FileInput(
                join=("sub",),
                where={
                    "datatype": "anat",
                    "suffix": "T1w",
                    "extension": ".nii.gz",
                    "dataset_id": "ds210",
                },
            )
        },
    )
    units = list(lake.bind(binding))
    assert units, "the anchor still resolves"
    assert all(u.inputs == {} for u in units)
    for u in units:
        assert [(x.name, x.n_matched, x.reason) for x in u.unresolved] == [("anat", 0, "missing")]


def test_ambiguous_input_is_distinguished_from_missing(lake) -> None:
    """Two failures, two fixes: *missing* means the unit is incomplete, *ambiguous*
    means the binding under-specifies. Collapsing them loses the distinction."""
    binding = Binding(
        anchor={**BOLD, "dataset_id": "ds001", "sub": "01"},
        key=("sub", "ses", "task", "run"),
        # Joining BOLD on `sub` alone matches all three of that subject's runs.
        inputs={"sibling": FileInput(join=("sub",), where={**BOLD, "dataset_id": "ds001"})},
    )
    units = list(lake.bind(binding))
    assert units
    for u in units:
        assert [(x.name, x.n_matched, x.reason) for x in u.unresolved] == [
            ("sibling", 3, "ambiguous")
        ]


def test_dataset_id_scopes_an_input(lake) -> None:
    """Subject labels are not unique across datasets, so an unscoped join is ambiguous.

    Both ``ds001`` and ``eyetracking_fmri`` have a ``sub-01``. Joining a T1w on ``sub``
    alone across the whole pool therefore matches two different people's anatomicals —
    a modelling error that is invisible if the resolver just takes the first hit.
    Reporting it as *ambiguous* is what turns it into something you can see and fix,
    and ``dataset_id`` is the fix.
    """
    anchor = {**BOLD, "dataset_id": "ds001", "sub": "01", "run": "01"}
    key = ("sub", "ses", "task", "run")

    unscoped = next(
        iter(
            lake.bind(
                Binding(
                    anchor=anchor,
                    key=key,
                    inputs={"anat": FileInput(join=("sub",), where=ANAT)},
                )
            )
        )
    )
    assert "anat" not in unscoped.inputs
    assert [(x.name, x.n_matched, x.reason) for x in unscoped.unresolved] == [
        ("anat", 2, "ambiguous")
    ]

    scoped = next(
        iter(
            lake.bind(
                Binding(
                    anchor=anchor,
                    key=key,
                    inputs={"anat": FileInput(join=("sub",), dataset_id="ds001", where=ANAT)},
                )
            )
        )
    )
    assert not scoped.unresolved
    assert "sub-01/anat" in str(scoped.inputs["anat"])


def test_table_input_returns_an_ordered_slice(lake) -> None:
    binding = Binding(
        anchor={**BOLD, "dataset_id": "ds001", "sub": "01", "run": "01"},
        key=("sub", "ses", "task", "run"),
        inputs={
            "events": TableInput(
                join=("sub", "task", "run"),
                table="events",
                columns=("onset", "duration"),
                order_by="row_idx",
            )
        },
    )
    unit = next(iter(lake.bind(binding)))
    events = unit.inputs["events"]
    assert events.columns == ["onset", "duration"]
    assert events.height == 158
    onsets = events["onset"].to_list()
    assert onsets == sorted(onsets), "row_idx order preserved"


def test_join_outside_the_key_is_rejected(lake) -> None:
    """An input can only join on entities the key provides -- otherwise the anchor has
    no value to match against, and the input would silently resolve against nothing."""
    binding = Binding(
        anchor={**BOLD, "dataset_id": "ds001"},
        key=("sub",),
        inputs={"anat": FileInput(join=("sub", "task"), where={"suffix": "T1w"})},
    )
    with pytest.raises(KeyError, match="task"):
        list(lake.bind(binding))


def test_unknown_column_names_the_table(lake) -> None:
    binding = Binding(
        anchor={**BOLD, "dataset_id": "ds001"},
        key=("sub",),
        inputs={"events": TableInput(join=("sub",), table="events", columns=("nope",))},
    )
    with pytest.raises(KeyError, match="events"):
        list(lake.bind(binding))


def test_resolution_cost_does_not_grow_with_units(lake, monkeypatch) -> None:
    """One query per declared input, not one per input per unit.

    This is what makes a binding usable on a real study: the hand-written form issues
    a query per sibling per unit, so cost scales with subjects x inputs.
    """
    calls: list[str] = []
    original = type(lake)._query
    monkeypatch.setattr(
        type(lake),
        "_query",
        lambda self, sql, params: calls.append(sql) or original(self, sql, params),
    )
    units = list(lake.bind(_ds001()))
    assert len(units) == 48
    # 1 anchor + 1 per input, plus whatever `get`/`resolve` need for root URIs --
    # the invariant is that it does not scale with the 48 units.
    assert len(calls) < 10, f"{len(calls)} queries for 48 units: {calls}"
