"""The generated types must *reject* bad queries.

The `ty` pre-commit hook already type-checks the package (the positive side).
What it cannot do is assert that an invalid query is rejected — a type-checker
gate fails *on* errors, it can't require that errors *exist*. Nor does the hook
passing prove the API is still typed: regressing `get()` to `**filters: Any`, or
`suffix` from `Suffix` to `str`, would keep the hook green while silently
dropping the safety. These invoke `ty` on a fixture of deliberately-bad
queries and assert they are flagged — so a loosened type surface fails CI.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

CRATE = Path(__file__).resolve().parents[1]
INVALID_USAGE = CRATE / "tests" / "typecheck" / "invalid_usage.py"
VENV = Path(sys.executable).resolve().parents[1]

#: The value-level `Literal`s. `ty` names the offending value in its diagnostic, so the
#: presence of each token is what proves the *value* was rejected and not merely the call.
BAD_VALUES = ["fnuc", "notarealsuffix"]


def _run_ty(path: Path) -> subprocess.CompletedProcess[str]:
    ty = Path(sys.executable).with_name("ty")
    if not ty.exists():
        pytest.skip("ty not installed in this environment")
    return subprocess.run(
        [str(ty), "check", "--python", str(VENV), str(path)],
        capture_output=True,
        text=True,
        cwd=CRATE,
    )


@pytest.fixture(scope="module")
def ty_output() -> str:
    """`ty`'s combined report for the bad-query fixture, and its exit status."""
    result = _run_ty(INVALID_USAGE)
    return f"exit={result.returncode}\n{result.stdout}{result.stderr}"


def test_bad_queries_are_rejected(ty_output: str) -> None:
    assert not ty_output.startswith("exit=0"), ty_output


@pytest.mark.parametrize("value", BAD_VALUES)
def test_the_rejected_value_is_named(ty_output: str, value: str) -> None:
    assert value in ty_output


@pytest.mark.xfail(
    strict=True,
    reason=(
        "ty does not currently reject an extra key in an Unpack'd TypedDict, so "
        "`get(tsk=...)` type-checks clean. When it does, drop this marker."
    ),
)
def test_an_unknown_filter_key_is_rejected(ty_output: str) -> None:
    """The third case `invalid_usage.py` declares — asserted, not assumed.

    Left as a strict xfail rather than deleted so that the fixture's claim and this file
    stay honest about which of its three usages are actually caught, and so the day `ty`
    gains the check the suite says so instead of quietly continuing to under-test.
    """
    assert "tsk" in ty_output
