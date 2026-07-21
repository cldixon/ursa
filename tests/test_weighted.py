"""Weighted algorithms end to end: weight= as an expression over edge columns.

`weight=` is a per-operation expression (`ur.col("amount")`, `ur.col("a") *
ur.col("b")`), evaluated in Rust against the edge table and gathered per CSR slot
via edge_ids. This covers weighted PageRank and weighted shortest_path (Dijkstra),
over in-memory and scan sources. Skipped if the native extension is not built.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges(table):
    return ur.from_arrow(table, src="src", dst="dst")


def test_weighted_pagerank_differs_from_unweighted():
    # 0 splits to 1 and 2; both point back to 0. Weighting 0->2 heavier lifts 2.
    tbl = pa.table(
        {
            "src": [0, 0, 1, 2],
            "dst": [1, 2, 0, 0],
            "amount": [1.0, 9.0, 1.0, 1.0],
        }
    )
    edges = _edges(tbl)
    unweighted = {r["id"]: r["pagerank"] for r in ur.pagerank(edges).collect().to_dicts()}
    weighted = {
        r["id"]: r["pr"]
        for r in edges.nodes()
        .with_columns(pr=ur.pagerank(edges, weight=ur.col("amount")))
        .collect()
        .to_dicts()
    }
    assert abs(unweighted[1] - unweighted[2]) < 1e-9  # symmetric unweighted
    assert weighted[2] > weighted[1]  # heavy 0->2 edge lifts node 2
    assert abs(sum(weighted.values()) - 1.0) < 1e-6


def test_weighted_expression_over_two_columns():
    # weight = amount * fx; the effective 0->2 weight (2*3=6) dominates 0->1 (5*1=5).
    tbl = pa.table(
        {
            "src": [0, 0, 1, 2],
            "dst": [1, 2, 0, 0],
            "amount": [5.0, 2.0, 1.0, 1.0],
            "fx": [1.0, 3.0, 1.0, 1.0],
        }
    )
    edges = _edges(tbl)
    rows = {
        r["id"]: r["pr"]
        for r in edges.nodes()
        .with_columns(pr=ur.pagerank(edges, weight=ur.col("amount") * ur.col("fx")))
        .collect()
        .to_dicts()
    }
    assert rows[2] > rows[1]


def test_weighted_shortest_path_picks_cheaper_route():
    # 0->1->3 (cost 1+1) vs direct 0->3 (cost 5). Unweighted BFS takes the 1 hop;
    # weighted Dijkstra takes the cheaper two-hop route.
    tbl = pa.table({"src": [0, 1, 0], "dst": [1, 3, 3], "cost": [1.0, 1.0, 5.0]})
    edges = _edges(tbl)
    unweighted = [r["src"] for r in ur.shortest_path(edges, 0, 3).collect().to_dicts()]
    assert unweighted == [0]  # single edge 0->3

    path = ur.shortest_path(edges, 0, 3, weight=ur.col("cost")).collect().to_dicts()
    assert [r["src"] for r in path] == [0, 1]
    assert [r["dst"] for r in path] == [1, 3]


def test_weighted_pagerank_over_a_scan_source(tmp_path):
    path = tmp_path / "edges.parquet"
    pq.write_table(
        pa.table({"src": [0, 0, 1, 2], "dst": [1, 2, 0, 0], "amount": [1.0, 9.0, 1.0, 1.0]}),
        path,
    )
    edges = ur.scan_edges(str(path), src="src", dst="dst")
    rows = {
        r["id"]: r["pr"]
        for r in edges.nodes()
        .with_columns(pr=ur.pagerank(edges, weight=ur.col("amount")))
        .collect()
        .to_dicts()
    }
    assert rows[2] > rows[1]


def test_negative_weight_errors():
    tbl = pa.table({"src": [0, 1], "dst": [1, 0], "w": [1.0, -3.0]})
    edges = _edges(tbl)
    with pytest.raises(Exception, match="non-negative"):
        edges.nodes().with_columns(pr=ur.pagerank(edges, weight=ur.col("w"))).collect()


def test_unknown_weight_column_errors():
    tbl = pa.table({"src": [0, 1], "dst": [1, 0], "w": [1.0, 2.0]})
    edges = _edges(tbl)
    with pytest.raises(ValueError, match="unknown edge column"):
        edges.nodes().with_columns(pr=ur.pagerank(edges, weight=ur.col("nope"))).collect()


def test_weight_still_rejected_for_deferred_algorithms():
    tbl = pa.table({"src": [0, 1], "dst": [1, 0], "w": [1.0, 2.0]})
    edges = _edges(tbl)
    for expr in (
        ur.closeness(edges, weight=ur.col("w")),
        ur.betweenness(edges, weight=ur.col("w")),
        ur.louvain(edges, weight=ur.col("w")),
    ):
        with pytest.raises(NotImplementedError):
            edges.nodes().with_columns(x=expr).collect()
