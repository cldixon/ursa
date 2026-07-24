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
    # Every section still runs (bit-rot guard across the whole example).
    for section in (
        "== standalone algorithms ==",
        "== composed pipeline ==",
        "== attribute enrichment + neighbour aggregation ==",
        "== 2-hop reachability from node 0 ==",
        "== shortest path 0 -> 5 ==",
        "== random walks from node 0 ==",
        "== weighted pagerank (weight=amount) ==",
        "== describe ==",
        "== expression dialect ==",
    ):
        assert section in out, f"missing section: {section}"
    # Known values off the fixed example graph (would flag a silent regression).
    assert "'triangle_count': 1" in out  # nodes in the triangle count 1
    assert "'betweenness': 11.0" in out  # node 0's exact betweenness
