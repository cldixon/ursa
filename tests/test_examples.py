"""Smoke test: the quickstart example runs end-to-end without error.

Keeps examples/quickstart.py from bit-rotting as the API evolves.
"""

import runpy
from pathlib import Path

import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `uv sync`)"
)

EXAMPLE = Path(__file__).resolve().parent.parent / "examples" / "quickstart.py"


def test_quickstart_runs(capsys):
    runpy.run_path(str(EXAMPLE), run_name="__main__")
    out = capsys.readouterr().out
    assert "standalone algorithms" in out
    assert "attribute enrichment" in out
