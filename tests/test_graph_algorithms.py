"""End-to-end tests for the node-valued graph algorithms wired in this change:
closeness, betweenness, label_propagation, and louvain.

Each is exercised through the public Python API (``ur.<algo>(edges)``) both as a
standalone NodeFrame and inside a ``with_columns`` pipeline, so the whole
Python → PyO3 → ursa-core path is covered. Skipped if the native extension has
not been built.
"""

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges(src, dst):
    return ur.from_arrow(pa.table({"src": src, "dst": dst}), src="src", dst="dst")


def _two_cliques():
    """Two 4-cliques {0,1,2,3} and {4,5,6,7} joined by one bridge 3-4."""
    pairs = [
        (0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3),
        (4, 5), (4, 6), (4, 7), (5, 6), (5, 7), (6, 7),
        (3, 4),
    ]  # fmt: skip
    src = [a for a, _ in pairs]
    dst = [b for _, b in pairs]
    return _edges(src, dst)


def test_closeness_directed_triangle():
    # 0->1->2->0: each node reaches the other two at distances {1, 2} -> 2/3.
    edges = _edges([0, 1, 2], [1, 2, 0])
    rows = {r["id"]: r["closeness"] for r in ur.closeness(edges).collect().to_dicts()}
    for node in (0, 1, 2):
        assert abs(rows[node] - 2.0 / 3.0) < 1e-12


def test_betweenness_directed_line():
    # 0->1->2->3->4: interior betweenness is [_, 3, 4, 3, _].
    edges = _edges([0, 1, 2, 3], [1, 2, 3, 4])
    df = edges.nodes().with_columns(bc=ur.betweenness(edges)).collect()
    got = {r["id"]: r["bc"] for r in df.to_dicts()}
    assert got[1] == 3.0
    assert got[2] == 4.0
    assert got[3] == 3.0
    assert got[0] == 0.0
    assert got[4] == 0.0


def test_label_propagation_recovers_communities():
    edges = _two_cliques()
    rows = ur.label_propagation(edges, seed=7).collect().to_dicts()
    labels = {r["id"]: r["label_propagation"] for r in rows}
    assert labels[0] == labels[1] == labels[2] == labels[3]
    assert labels[4] == labels[5] == labels[6] == labels[7]
    assert labels[0] != labels[4]


def test_louvain_recovers_communities():
    edges = _two_cliques()
    comm = {r["id"]: r["louvain"] for r in ur.louvain(edges, seed=1).collect().to_dicts()}
    assert comm[0] == comm[1] == comm[2] == comm[3]
    assert comm[4] == comm[5] == comm[6] == comm[7]
    assert comm[0] != comm[4]


def test_algorithms_compose_in_one_pipeline():
    # All four share a single topology build over one frame (index-preservation).
    edges = _two_cliques()
    df = (
        edges.nodes()
        .with_columns(
            c=ur.closeness(edges),
            bc=ur.betweenness(edges),
            lp=ur.label_propagation(edges, seed=7),
            lv=ur.louvain(edges, seed=1),
        )
        .collect()
    )
    assert set(df.columns) == {"id", "c", "bc", "lp", "lv"}
    assert len(df) == 8


def test_weighted_variants_run():
    # closeness/betweenness/louvain accept weight= (see tests/test_weighted.py for
    # the value assertions); here just confirm they execute over a weighted frame.
    edges = ur.from_arrow(
        pa.table({"src": [0, 1, 2], "dst": [1, 2, 0], "w": [1.0, 1.0, 1.0]}), src="src", dst="dst"
    )
    df = (
        edges.nodes()
        .with_columns(
            c=ur.closeness(edges, weight=ur.col("w")),
            bc=ur.betweenness(edges, weight=ur.col("w")),
            lv=ur.louvain(edges, weight=ur.col("w"), seed=1),
        )
        .collect()
    )
    assert set(df.columns) == {"id", "c", "bc", "lv"}
