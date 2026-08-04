"""frame.join(other, on=, how=) — the relational join verb (issue #18).

A join between two arbitrary frames (distinct from the automatic attribute attach
that ``edges.nodes().with_columns(...)`` does). v1 supports inner/left joins on a
shared key; the result keeps the left frame's type. The whole relational tail
(filter/sort/head/distinct/sample/rename/select) applies after the join. Both
sides materialize to Arrow and the join runs in DataFusion.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _nodes(ids, **cols):
    return ur.from_arrow(pa.table({"id": ids, **cols}), id="id")


def _rows(frame):
    d = frame.collect().to_arrow().to_pydict()
    n = len(next(iter(d.values())))
    return [{k: d[k][i] for k in d} for i in range(n)]


# --- inner / left ----------------------------------------------------------


def test_inner_join_by_id():
    left = _nodes([0, 1, 2], a=[10, 11, 12])
    right = _nodes([1, 2, 3], b=[21, 22, 23])
    out = left.join(right, on="id").collect().to_arrow()
    assert out.num_rows == 2  # ids 1, 2 only
    assert set(out.column_names) == {"id", "a", "b"}  # single id column
    rows = {r["id"]: r for r in out.to_pylist()}
    assert rows[1] == {"id": 1, "a": 11, "b": 21}
    assert rows[2] == {"id": 2, "a": 12, "b": 22}


def test_left_join_keeps_all_left_rows():
    left = _nodes([0, 1, 2], a=[10, 11, 12])
    right = _nodes([1, 2], b=[21, 22])
    out = left.join(right, on="id", how="left").collect().to_arrow()
    assert out.num_rows == 3
    rows = {r["id"]: r for r in out.to_pylist()}
    assert rows[0]["b"] is None  # unmatched -> null
    assert rows[1]["b"] == 21


def test_join_result_keeps_left_frame_type():
    left = _nodes([0, 1], a=[1, 2])
    right = _nodes([0, 1], b=[3, 4])
    joined = left.join(right, on="id")
    assert isinstance(joined, ur.NodeFrame)


# --- key forms & multi-key -------------------------------------------------


def test_on_accepts_str_and_col():
    left = _nodes([0, 1], a=[1, 2])
    right = _nodes([0, 1], b=[3, 4])
    a = left.join(right, on="id").collect().to_arrow().to_pydict()
    b = left.join(right, on=ur.col("id")).collect().to_arrow().to_pydict()
    assert a == b


def test_multi_key_join():
    left = ur.from_arrow(pa.table({"id": [0, 0, 1], "k": ["x", "y", "x"], "a": [1, 2, 3]}), id="id")
    right = ur.from_arrow(pa.table({"id": [0, 1], "k": ["x", "x"], "b": [9, 8]}), id="id")
    out = left.join(right, on=["id", "k"]).collect().to_arrow()
    rows = {(r["id"], r["k"]): r for r in out.to_pylist()}
    assert set(rows) == {(0, "x"), (1, "x")}  # (0,'y') has no right match
    assert rows[(0, "x")]["b"] == 9


def test_string_id_join():
    left = ur.from_arrow(pa.table({"id": ["u1", "u2"], "a": [1, 2]}), id="id")
    right = ur.from_arrow(pa.table({"id": ["u2", "u3"], "b": [5, 6]}), id="id")
    out = left.join(right, on="id").collect().to_arrow()
    assert out.to_pylist() == [{"id": "u2", "a": 2, "b": 5}]


# --- composition with the tail ---------------------------------------------


def test_join_composes_with_filter_sort_head():
    left = _nodes([0, 1, 2, 3], a=[5, 1, 9, 3])
    right = _nodes([0, 1, 2, 3], b=[1, 1, 1, 1])
    out = (
        left.join(right, on="id")
        .filter(ur.col("a") > 2)
        .sort("a", descending=True)
        .head(2)
        .collect()
        .to_arrow()
    )
    assert out.column("a").to_pylist() == [9, 5]  # a>2 -> {5,9,3}; top 2 desc


def test_join_composes_with_rename_and_select():
    left = _nodes([0, 1], a=[1, 2])
    right = _nodes([0, 1], b=[3, 4])
    out = left.join(right, on="id").rename({"b": "bb"}).select("id", "bb").collect().to_arrow()
    assert out.column_names == ["id", "bb"]
    assert out.num_rows == 2


def test_post_join_filter_on_right_column():
    left = _nodes([0, 1, 2], a=[1, 2, 3])
    right = _nodes([0, 1, 2], region=["us", "eu", "us"])
    out = left.join(right, on="id").filter(ur.col("region") == "us").collect().to_arrow()
    assert set(out.column("id").to_pylist()) == {0, 2}


# --- graph-derived left ----------------------------------------------------


def test_graph_derived_left_join():
    # Compute pagerank per node, then join a region attribute table and aggregate.
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")
    regions = _nodes([0, 1, 2, 3], region=["us", "us", "eu", "eu"])
    out = (
        edges.nodes()
        .with_columns(pr=ur.pagerank(edges))
        .join(regions, on="id")
        .sort("pr", descending=True)
        .collect()
        .to_arrow()
    )
    assert out.num_rows == 4
    assert {"id", "pr", "region"} <= set(out.column_names)
    prs = out.column("pr").to_pylist()
    assert prs == sorted(prs, reverse=True)


# --- edge-frame left -------------------------------------------------------


def test_edge_frame_join_enriches_edges():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    # A per-source attribute table (a NodeFrame keyed on the endpoint column "s"),
    # joined onto the edges by that column.
    attrs = ur.from_arrow(pa.table({"s": [0, 1, 2], "kind": ["a", "b", "c"]}), id="s")
    out = edges.join(attrs, on="s").collect().to_arrow()
    assert out.num_rows == 3
    assert {"s", "d", "kind"} <= set(out.column_names)


def test_graph_op_after_edge_join_errors():
    edges = ur.from_arrow(pa.table({"s": [0, 1], "d": [1, 2]}), src="s", dst="d")
    other = ur.from_arrow(pa.table({"s": [0, 1], "x": [1, 2]}), id="s")
    joined = edges.join(other, on="s")
    # A graph algorithm on a joined (derived) edge frame must error clearly, not
    # build a wrong topology.
    with pytest.raises(Exception):  # noqa: B017
        ur.pagerank(joined).collect()


# --- error surface ---------------------------------------------------------


def test_join_on_none_errors():
    left = _nodes([0, 1], a=[1, 2])
    right = _nodes([0, 1], b=[3, 4])
    with pytest.raises(NotImplementedError, match="on="):
        left.join(right).collect()


def test_join_unsupported_how_errors():
    left = _nodes([0, 1], a=[1, 2])
    right = _nodes([0, 1], b=[3, 4])
    with pytest.raises(NotImplementedError, match="not supported"):
        left.join(right, on="id", how="outer").collect()


def test_join_missing_key_errors():
    left = _nodes([0, 1], a=[1, 2])
    right = ur.from_arrow(pa.table({"other": [0, 1], "b": [3, 4]}), id="other")
    with pytest.raises(Exception, match="key"):
        left.join(right, on="id").collect()


def test_non_key_column_collision_errors():
    left = _nodes([0, 1], a=[1, 2])
    right = _nodes([0, 1], a=[3, 4])  # both have a non-key column 'a'
    with pytest.raises(ValueError, match="duplicate column"):
        left.join(right, on="id").collect()


def test_other_not_a_frame_errors():
    left = _nodes([0, 1], a=[1, 2])
    with pytest.raises(TypeError, match="ursa frame"):
        left.join(pa.table({"id": [0, 1], "b": [3, 4]}), on="id").collect()


def test_double_join_errors():
    left = _nodes([0, 1], a=[1, 2])
    r1 = _nodes([0, 1], b=[3, 4])
    r2 = _nodes([0, 1], c=[5, 6])
    with pytest.raises(NotImplementedError, match="single join"):
        left.join(r1, on="id").join(r2, on="id").collect()


def test_join_after_group_by_errors():
    left = _nodes([0, 1], region=["us", "us"], v=[1, 2])
    right = _nodes([0, 1], b=[3, 4])
    with pytest.raises(NotImplementedError):
        (left.group_by("region").agg(total=ur.col("v").sum()).join(right, on="id").collect())


def test_join_on_traversal_result_errors():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    attrs = ur.from_arrow(pa.table({"src": [0, 1], "x": [1, 2]}), id="src")
    with pytest.raises(NotImplementedError, match="traversal"):
        ur.hop(edges, 1).from_([0]).join(attrs, on="src").collect()


# --- #100: a derived (filtered/sampled) edge frame now collects -------------
# The same source-retention change that lets edges.join(...).collect() work also
# makes filter/sample/distinct on a plain edge frame materializable (a graph op on
# such a frame still errors clearly — verified in test_honest_collect).


def test_filtered_edge_frame_collects():
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")
    out = edges.filter(ur.dst() == 0).collect().to_arrow()
    assert out.num_rows == 3  # three edges point at 0
    assert set(out.column("d").to_pylist()) == {0}


def test_sampled_edge_frame_collects():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2, 3], "d": [1, 2, 3, 0]}), src="s", dst="d")
    out = edges.sample(2, seed=7).collect().to_arrow()
    assert out.num_rows == 2


# --- the derived-frame guard is authoritative (must fire even when the index
#     cell was pre-populated by another path) ---------------------------------


def test_weighted_algo_on_derived_scan_frame_still_guards(tmp_path):
    # A weighted algorithm builds the index directly from the scan; on a *derived*
    # frame that must still raise the guard, not silently use the pre-derivation
    # topology (regression: the source-retention change made this path reachable).
    import pyarrow.parquet as pq

    path = tmp_path / "e.parquet"
    pq.write_table(
        pa.table({"s": [0, 1, 2, 0], "d": [1, 2, 0, 1], "w": [1.0, 1.0, 1.0, 2.0]}), path
    )
    derived = ur.scan_edges(str(path), src="s", dst="d").distinct()
    with pytest.raises(NotImplementedError, match="filtered/derived"):
        derived.nodes().with_columns(pr=ur.pagerank(derived, weight=ur.col("w"))).collect()


def test_graph_op_on_grouped_edge_frame_guards():
    # A grouped edge frame drops the index (fresh cell), so a graph op on it hits
    # the guard even after the parent frame's index was already built.
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    edges.nodes().with_columns(deg=ur.degree(edges)).collect()  # build parent index
    grouped = edges.group_by("s").agg(c=ur.col("d").count())
    with pytest.raises(NotImplementedError, match="filtered/derived"):
        ur.pagerank(grouped).collect()  # ty: ignore[invalid-argument-type]


# --- oracle ----------------------------------------------------------------


def test_matches_pyarrow_join_reference():
    left_t = pa.table({"id": [0, 1, 2, 3], "a": [10, 11, 12, 13]})
    right_t = pa.table({"id": [1, 2, 4], "b": [21, 22, 24]})
    out = ur.from_arrow(left_t, id="id").join(ur.from_arrow(right_t, id="id"), on="id")
    ours = {r["id"]: r for r in _rows(out)}
    ref = left_t.join(right_t, keys="id", join_type="inner")
    theirs = {r["id"]: r for r in ref.to_pylist()}
    assert ours == theirs
