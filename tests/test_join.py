"""frame.join(other, on=, how=) — the relational join verb (issue #18).

A join between two arbitrary frames (distinct from the automatic attribute attach
that ``edges.nodes().with_columns(...)`` does). v1 supports inner/left joins on a
shared key; the result keeps the left frame's type. The whole relational tail
(filter/sort/head/distinct/sample/rename/select) applies after the join. Both
sides materialize to Arrow and the join runs in DataFusion. A join composes with
group_by().agg() in either order, as written.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.compute as pc
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


def test_join_after_group_by_on_a_dropped_key_errors():
    # group_by().agg() replaces the schema with [keys..., outputs...], so joining
    # the grouped result on the (now gone) id names the missing key and the columns
    # that are there.
    left = _nodes([0, 1], region=["us", "us"], v=[1, 2])
    right = _nodes([0, 1], b=[3, 4])
    with pytest.raises(ur.ColumnNotFoundError, match=r"'id' not in the left frame"):
        left.group_by("region").agg(total=ur.col("v").sum()).join(right, on="id").collect()


def test_join_on_traversal_result_errors():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    attrs = ur.from_arrow(pa.table({"src": [0, 1], "x": [1, 2]}), id="src")
    with pytest.raises(NotImplementedError, match="traversal"):
        ur.hop(edges, 1).from_([0]).join(attrs, on="src").collect()


# --- join composed with group_by().agg() -----------------------------------
# Step order is honored as written: join().group_by() groups the joined rows;
# group_by().agg().join() joins onto the grouped result. Each test checks the
# values against a pyarrow join/group_by of the same tables.


SALES = pa.table({"id": [0, 1, 2, 3, 4], "v": [10, 20, 30, 40, 50]})
REGIONS = pa.table({"id": [0, 1, 2, 3], "region": ["us", "us", "eu", "eu"]})  # no id 4


def test_join_then_group_by_on_node_frame():
    out = (
        ur.from_arrow(SALES, id="id")
        .join(ur.from_arrow(REGIONS, id="id"), on="id")
        .group_by("region")
        .agg(total=ur.col("v").sum(), n=ur.col("v").count())
        .collect()
        .to_arrow()
    )
    ref = (
        SALES.join(REGIONS, keys="id", join_type="inner")
        .group_by("region")
        .aggregate([("v", "sum"), ("v", "count")])
    )
    theirs = {r["region"]: (r["v_sum"], r["v_count"]) for r in ref.to_pylist()}
    assert theirs == {"us": (30, 2), "eu": (70, 2)}
    assert {r["region"]: (r["total"], r["n"]) for r in out.to_pylist()} == theirs


def test_join_then_group_by_with_filter_and_tail():
    # A filter (before the group) applies to the joined rows; sort/head/rename
    # after the group apply to the grouped result. A left join keeps id 4 with a
    # null region, which groups as its own key.
    out = (
        ur.from_arrow(SALES, id="id")
        .join(ur.from_arrow(REGIONS, id="id"), on="id", how="left")
        .filter(ur.col("v") > 10)
        .group_by("region")
        .agg(total=ur.col("v").sum())
        .sort("total", descending=True)
        .head(2)
        .rename({"total": "revenue"})
        .collect()
        .to_arrow()
    )
    assert out.column_names == ["region", "revenue"]
    assert out.to_pylist() == [
        {"region": "eu", "revenue": 70},
        {"region": None, "revenue": 50},
    ]


def test_join_then_group_by_on_graph_derived_node_frame():
    # The notebook case: a graph algorithm, an attribute join, then a per-club rollup.
    edges = ur.from_arrow(
        pa.table({"s": [1, 2, 3, 0, 4, 5], "d": [0, 0, 0, 1, 5, 4]}), src="s", dst="d"
    )
    attrs = ur.from_arrow(
        pa.table({"id": [0, 1, 2, 3, 4, 5], "club": ["a", "a", "a", "b", "b", "b"]}), id="id"
    )
    out = (
        edges.nodes()
        .with_columns(pagerank=ur.pagerank(edges))
        .join(attrs, on="id")
        .group_by("club")
        .agg(pr_sum=ur.col("pagerank").sum(), members=ur.col("id").count())
        .sort("club")
        .collect()
        .to_arrow()
    )
    # Reference: the same pagerank collected on its own, joined + grouped in pyarrow.
    pr = edges.nodes().with_columns(pagerank=ur.pagerank(edges)).collect().to_arrow()
    ref = (
        pr.join(attrs.collect().to_arrow(), keys="id")
        .group_by("club")
        .aggregate([("pagerank", "sum"), ("id", "count")])
        .sort_by("club")
    )
    assert out.column("club").to_pylist() == ["a", "b"]
    assert out.column("members").to_pylist() == ref.column("id_count").to_pylist() == [3, 3]
    assert out.column("pr_sum").to_pylist() == pytest.approx(ref.column("pagerank_sum").to_pylist())


def test_group_by_then_join_on_node_frame():
    # The grouped result (region, total) is the left side; the join adds a
    # per-region attribute. A sort after the join orders the joined rows.
    info = ur.from_arrow(pa.table({"region": ["us", "eu"], "manager": ["ann", "bo"]}), id="region")
    regional = _nodes([0, 1, 2, 3], region=["us", "us", "eu", "eu"], v=[10, 20, 30, 40])
    out = (
        regional.group_by("region")
        .agg(total=ur.col("v").sum())
        .join(info, on="region")
        .sort("total")
        .collect()
        .to_arrow()
    )
    assert out.to_pylist() == [
        {"region": "us", "total": 30, "manager": "ann"},
        {"region": "eu", "total": 70, "manager": "bo"},
    ]


def test_group_by_then_join_splits_the_tail_at_the_join():
    # head/rename written *between* the group and the join shape the grouped side
    # (the rename even supplies the join key); a sort after the join orders its rows.
    nodes = ur.from_arrow(
        pa.table({"id": [0, 1, 2, 3, 4], "g": ["x", "x", "y", "y", "z"], "v": [1, 2, 3, 4, 5]}),
        id="id",
    )
    label = ur.from_arrow(pa.table({"key": ["x", "y", "z"], "name": ["X", "Y", "Z"]}), id="key")
    out = (
        nodes.group_by("g")
        .agg(total=ur.col("v").sum())
        .sort("total", descending=True)
        .head(2)  # keeps y (7) and z (5), drops x (3) before the join
        .rename({"g": "key"})
        .join(label, on="key")
        .sort("key")
        .collect()
        .to_arrow()
    )
    assert out.to_pylist() == [
        {"key": "y", "total": 7, "name": "Y"},
        {"key": "z", "total": 5, "name": "Z"},
    ]


def test_group_by_then_join_on_graph_derived_node_frame():
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")
    nodes = _nodes([0, 1, 2, 3], region=["us", "us", "eu", "eu"])
    info = ur.from_arrow(pa.table({"region": ["us", "eu"], "hq": ["nyc", "ber"]}), id="region")
    out = (
        nodes.with_columns(deg=ur.degree(edges, direction="in"))
        .group_by("region")
        .agg(indeg=ur.col("deg").sum())
        .join(info, on="region")
        .sort("region")
        .collect()
        .to_arrow()
    )
    # In-degrees by hand: node0=3, node1=1, node2=0, node3=0.
    assert out.to_pylist() == [
        {"region": "eu", "indeg": 0, "hq": "ber"},
        {"region": "us", "indeg": 4, "hq": "nyc"},
    ]


def test_join_then_group_by_on_edge_frame():
    edges = ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2, 2, 2], "d": [1, 2, 2, 0, 1, 3], "w": [1, 2, 3, 4, 5, 6]}),
        src="s",
        dst="d",
    )
    kinds = pa.table({"s": [0, 1, 2], "kind": ["a", "b", "a"]})
    out = (
        edges.join(ur.from_arrow(kinds, id="s"), on="s")
        .filter(ur.dst() != 3)
        .group_by("kind")
        .agg(w=ur.col("w").sum(), n=ur.col("d").count())
        .sort("kind")
        .collect()
        .to_arrow()
    )
    ref = (
        edges.collect()
        .to_arrow()
        .join(kinds, keys="s")
        .filter(pc.field("d") != 3)
        .group_by("kind")
        .aggregate([("w", "sum"), ("d", "count")])
        .sort_by("kind")
    )
    assert out.to_pylist() == [
        {"kind": k, "w": w, "n": n}
        for k, w, n in zip(
            ref.column("kind").to_pylist(),
            ref.column("w_sum").to_pylist(),
            ref.column("d_count").to_pylist(),
            strict=True,
        )
    ]
    assert out.to_pylist() == [{"kind": "a", "w": 12, "n": 4}, {"kind": "b", "w": 3, "n": 1}]


def test_group_by_then_join_on_edge_frame():
    # out-degree per source, then a per-source label joined onto the groups.
    edges = ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2, 2, 2], "d": [1, 2, 2, 0, 1, 3]}), src="s", dst="d"
    )
    names = ur.from_arrow(pa.table({"s": [0, 1, 2], "name": ["n0", "n1", "n2"]}), id="s")
    out = (
        edges.group_by("s")
        .agg(deg=ur.col("d").count())
        .join(names, on="s")
        .sort("deg", descending=True)
        .head(2)
        .collect()
        .to_arrow()
    )
    assert out.to_pylist() == [
        {"s": 2, "deg": 3, "name": "n2"},
        {"s": 0, "deg": 2, "name": "n0"},
    ]


def test_join_then_head_then_group_by_applies_in_written_order():
    # The sort/head between the join and the group apply to the joined rows before
    # grouping: the two lowest ids (0, 1) are both us -> one group of 10 + 20.
    left = ur.from_arrow(SALES, id="id")
    out = (
        left.join(ur.from_arrow(REGIONS, id="id"), on="id")
        .sort("id")
        .head(2)
        .group_by("region")
        .agg(total=ur.col("v").sum())
        .collect()
        .to_dicts()
    )
    assert out == [{"region": "us", "total": 30}]


def test_rename_then_join_on_the_new_key():
    # A rename before the join takes effect before it, so the join can key on the
    # new name (#164); pyarrow's join of the renamed table is the reference.
    sales = SALES.rename_columns(["node", "v"])
    regions = REGIONS.rename_columns(["node", "region"])
    out = (
        ur.from_arrow(SALES, id="id")
        .rename({"id": "node"})
        .join(ur.from_arrow(regions, id="node"), on="node")
        .sort("node")
        .collect()
        .to_arrow()
    )
    ref = sales.join(regions, keys="node", join_type="inner").sort_by("node")
    assert out.to_pylist() == ref.select(out.column_names).to_pylist()
    assert out.to_pylist() == [
        {"node": 0, "v": 10, "region": "us"},
        {"node": 1, "v": 20, "region": "us"},
        {"node": 2, "v": 30, "region": "eu"},
        {"node": 3, "v": 40, "region": "eu"},
    ]


def test_head_before_join_limits_the_left_side():
    # head(2) before the join keeps the two highest-v rows (ids 4, 3); id 4 has no
    # region, so the inner join keeps one row. (Previously the head ran after the
    # join and returned two.)
    out = (
        ur.from_arrow(SALES, id="id")
        .sort("v", descending=True)
        .head(2)
        .join(ur.from_arrow(REGIONS, id="id"), on="id")
        .collect()
        .to_dicts()
    )
    assert out == [{"id": 3, "v": 40, "region": "eu"}]


def test_join_with_group_by_on_traversal_result_errors():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    attrs = ur.from_arrow(pa.table({"src": [0, 1], "x": [1, 2]}), id="src")
    with pytest.raises(NotImplementedError, match="traversal"):
        (
            ur.hop(edges, 1)
            .from_([0])
            .join(attrs, on="src")
            .group_by("x")
            .agg(n=ur.col("src").count())
            .collect()
        )


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
    with pytest.raises(NotImplementedError, match="distinct/sample/join/group_by"):
        derived.nodes().with_columns(pr=ur.pagerank(derived, weight=ur.col("w"))).collect()


def test_graph_op_on_grouped_edge_frame_guards():
    # A grouped edge frame drops the index (fresh cell), so a graph op on it hits
    # the guard even after the parent frame's index was already built.
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    edges.nodes().with_columns(deg=ur.degree(edges)).collect()  # build parent index
    grouped = edges.group_by("s").agg(c=ur.col("d").count())
    with pytest.raises(NotImplementedError, match="distinct/sample/join/group_by"):
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
