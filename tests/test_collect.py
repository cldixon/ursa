"""End-to-end tests of the collect() walking-skeleton slice.

Python → PyO3 → ursa-plan (DataFusion ExecutionPlan) → ursa-core kernel →
Arrow → Python. Skipped if the native extension isn't built.
"""

import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `uv sync`)"
)

pa = pytest.importorskip("pyarrow")


def diamond_edges() -> ur.EdgeFrame:
    # 0->1, 0->2, 1->2, 2->0
    tbl = pa.table(
        {"s": pa.array([0, 0, 1, 2], pa.int64()), "d": pa.array([1, 2, 2, 0], pa.int64())}
    )
    return ur.from_arrow(tbl, src="s", dst="d")


def test_pagerank_collect_to_arrow():
    edges = diamond_edges()
    result = ur.pagerank(edges).collect()
    assert isinstance(result, ur.MaterializedFrame)
    tbl = result.to_arrow()
    assert tbl.column_names == ["id", "pagerank"]
    assert tbl.num_rows == 3
    total = sum(tbl.column("pagerank").to_pylist())
    assert abs(total - 1.0) < 1e-6


def test_degree_collect_to_dicts():
    edges = diamond_edges()
    rows = ur.degree(edges, direction="out").collect().to_dicts()
    by_id = {r["id"]: r["degree"] for r in rows}
    assert by_id == {0: 2, 1: 1, 2: 1}


def test_connected_components_collect():
    tbl = pa.table({"s": pa.array([0, 1, 3], pa.int64()), "d": pa.array([1, 2, 4], pa.int64())})
    edges = ur.from_arrow(tbl, src="s", dst="d")
    rows = ur.connected_components(edges).collect().to_dicts()
    # standalone columns are named after the algorithm (verb == column name)
    label = {r["id"]: r["connected_components"] for r in rows}
    assert label[0] == label[1] == label[2]
    assert label[3] == label[4]
    assert label[0] != label[3]


def test_to_polars_egress():
    polars = pytest.importorskip("polars")
    edges = diamond_edges()
    df = ur.pagerank(edges).to_polars()
    assert isinstance(df, polars.DataFrame)
    assert df.columns == ["id", "pagerank"]
    assert abs(df["pagerank"].sum() - 1.0) < 1e-6


def test_collect_over_unsupported_scan_format_errors_clearly():
    # scan_edges resolves .parquet/.csv; an unsupported extension errors clearly.
    edges = ur.scan_edges("edges.json", src="a", dst="b")
    with pytest.raises(RuntimeError, match="supports"):
        ur.pagerank(edges).collect()


def test_unwired_algorithm_errors_clearly():
    # A verb with no kernel behind it still fails clearly (the `_EXECUTABLE`
    # guard). Built directly since every public node verb is now wired.
    from ursa._graph import _graph_expr

    edges = diamond_edges()
    future = _graph_expr("some_future_algorithm", edges=edges)
    with pytest.raises(NotImplementedError, match="not wired"):
        future.collect()
