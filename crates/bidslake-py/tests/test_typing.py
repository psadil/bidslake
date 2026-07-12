"""The generated types must *reject* bad queries.

The `ty` pre-commit hook already type-checks the package (the positive side).
What it cannot do is assert that an invalid query is rejected — a type-checker
gate fails *on* errors, it can't require that errors *exist*. Nor does the hook
passing prove the API is still typed: regressing `get()` to `**filters: Any`, or
`suffix` from `Suffix` to `str`, would keep the hook green while silently
dropping the safety. This test invokes `ty` on a fixture of deliberately-bad
queries and asserts they are flagged — so a loosened type surface fails CI.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

import pytest

CRATE = Path(__file__).resolve().parents[1]
INVALID_USAGE = CRATE / "tests" / "typecheck" / "invalid_usage.py"
VENV = Path(sys.executable).resolve().parents[1]


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


def test_bad_queries_are_type_errors():
    result = _run_ty(INVALID_USAGE)
    out = result.stdout + result.stderr
    # Rejected overall, and the value-level Literals flag the bad datatype/suffix.
    assert result.returncode != 0 and "fnuc" in out and "notarealsuffix" in out
