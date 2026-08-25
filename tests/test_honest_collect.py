"""collect() is honest — the #33-#36/#48 cluster from the 2026-07-21 review.

Every accepted step/parameter is either honored or raises; nothing is silently
dropped. Also covers the newly-wired plain-source collect (read_*/scan->to_*).
Native-gated where the kernel path is exercised.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

native = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges():
    tbl = pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0]})
    return ur.from_arrow(tbl, src="s", dst="d")


# --- #18: rename now executes (was a dropped/raising step) -----------------
@native
def test_rename_relabels_output_column():
    edges = _edges()
    out = (
        edges.nodes()
        .with_columns(pr=ur.pagerank(edges))
        .rename({"pr": "score"})
        .collect()
        .to_arrow()
    )
    assert "score" in out.column_names
    assert "pr" not in out.column_names


@native
def test_rename_unknown_column_raises():
    edges = _edges()
    frame = edges.nodes().with_columns(pr=ur.pagerank(edges)).rename({"nope": "x"})
    with pytest.raises(Exception, match="rename"):
        frame.collect()


@native
def test_select_on_traversal_narrows_output():
    # select() is now implemented (Polars-faithful output projection); on a
    # traversal it narrows the (src, dst) result to the chosen columns.
    edges = _edges()
    df = ur.hop(edges, 2).from_([0]).select("dst").collect().to_polars()
    assert df.columns == ["dst"]


@native
def test_connected_components_strong_partitions_by_reachability():
    # cycle {0,1,2} plus a one-way edge 2->3: weakly one component, but strongly
    # two — {0,1,2} are mutually reachable, {3} is a sink of its own.
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2, 2], "d": [1, 2, 0, 3]}), src="s", dst="d")
    strong = {
        r["id"]: r["connected_components"]
        for r in ur.connected_components(edges, mode="strong").collect().to_dicts()
    }
    assert strong[0] == strong[1] == strong[2]
    assert strong[3] != strong[0]
    assert len(set(strong.values())) == 2
    # ...whereas weak lumps all four together.
    weak = {r["connected_components"] for r in ur.connected_components(edges).collect().to_dicts()}
    assert len(weak) == 1


@native
def test_connected_components_weak_still_works():
    edges = _edges()
    rows = ur.connected_components(edges).collect().to_dicts()
    # all four nodes are one weakly-connected component
    assert len({r["connected_components"] for r in rows}) == 1


@native
def test_neighbors_from_raises():
    edges = _edges()
    other = ur.from_arrow(pa.table({"id": [0, 1], "v": [1.0, 2.0]}), id="id")
    expr = ur.neighbors(edges, from_=other).agg(ur.col("v").mean())
    with pytest.raises(NotImplementedError, match="from_"):
        edges.nodes().with_columns(x=expr).collect()


def test_scan_format_opts_raise_at_construction():
    with pytest.raises(NotImplementedError, match="format options"):
        ur.scan_edges("edges.csv", src="s", dst="d", delimiter=";")
    with pytest.raises(NotImplementedError, match="format options"):
        ur.scan_nodes("nodes.csv", id="id", has_header=False)


def test_sink_csv_rejects_dropped_opts():
    # **opts were silently discarded; the parameter is gone, so passing one is a
    # clear TypeError rather than a silent no-op.
    edges = _edges()
    with pytest.raises(TypeError):
        ur.pagerank(edges).sink_csv("out.csv", delimiter=";")  # ty: ignore[unknown-argument]


@native
def test_describe_tail_raises():
    edges = _edges()
    with pytest.raises(NotImplementedError, match="describe"):
        ur.describe(edges).head(3).collect()


# --- #35: reverse() carries the source -------------------------------------
@native
def test_reverse_carries_source_degree_transpose():
    edges = _edges()
    out_on_rev = {
        r["id"]: r["degree"]
        for r in ur.degree(edges.reverse(), direction="out").collect().to_dicts()
    }
    in_on_orig = {
        r["id"]: r["degree"] for r in ur.degree(edges, direction="in").collect().to_dicts()
    }
    assert out_on_rev == in_on_orig


@native
def test_reverse_scan_source(tmp_path):
    path = tmp_path / "e.parquet"
    pq.write_table(pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0]}), path)
    edges = ur.scan_edges(str(path), src="s", dst="d")
    out_on_rev = {
        r["id"]: r["degree"]
        for r in ur.degree(edges.reverse(), direction="out").collect().to_dicts()
    }
    in_on_orig = {
        r["id"]: r["degree"] for r in ur.degree(edges, direction="in").collect().to_dicts()
    }
    assert out_on_rev == in_on_orig


@native
def test_filtered_edge_frame_is_a_subgraph_view():
    # #114: a graph op over a *filtered* edge frame is a subgraph view — the parent
    # topology runs restricted by an edge mask (dropped rows ride along), not a
    # rebuild and not a raise. Here `s > 0` drops both edges leaving node 0, so 0
    # keeps its incoming edges (in-degree 2) but has out-degree 0.
    edges = _edges()
    full = {r["id"]: r["degree"] for r in ur.degree(edges, direction="out").collect().to_dicts()}
    assert full == {0: 2, 1: 1, 2: 1}
    sub = {
        r["id"]: r["degree"]
        for r in ur.degree(edges.filter(ur.col("s") > 0), direction="out").collect().to_dicts()
    }
    # Node 0 loses its two outgoing edges; every node stays present (degree 0).
    assert sub == {0: 0, 1: 1, 2: 1}


# --- #36: plain-source collect (read_*/scan->to_*) --------------------------
@native
def test_read_edges_roundtrip_parquet(tmp_path):
    path = tmp_path / "e.parquet"
    pq.write_table(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), path)
    df = ur.read_edges(str(path), src="s", dst="d")
    assert set(df.columns) == {"s", "d"}
    rows = sorted((r["s"], r["d"]) for r in df.to_dicts())
    assert rows == [(0, 1), (1, 2), (2, 0)]


@native
def test_scan_edges_to_polars(tmp_path):
    path = tmp_path / "e.parquet"
    pq.write_table(pa.table({"s": [0, 1], "d": [1, 0]}), path)
    df = ur.scan_edges(str(path), src="s", dst="d").to_polars()
    assert df.shape == (2, 2)


@native
def test_from_arrow_node_plain_collect():
    nodes = ur.from_arrow(pa.table({"id": [1, 2, 3], "attr": [10, 20, 30]}), id="id")
    rows = nodes.collect().to_dicts()
    assert rows == [{"id": 1, "attr": 10}, {"id": 2, "attr": 20}, {"id": 3, "attr": 30}]


@native
def test_plain_scan_with_head_tail(tmp_path):
    path = tmp_path / "n.parquet"
    pq.write_table(pa.table({"id": [1, 2, 3, 4], "v": [9, 8, 7, 6]}), path)
    got = ur.scan_nodes(str(path), id="id").head(2).collect()
    assert len(got) == 2


@native
def test_edges_nodes_plain_collect_raises_clearly():
    # bare edges.nodes() has no materializable node set yet — honest message.
    edges = _edges()
    with pytest.raises(NotImplementedError, match="node set"):
        edges.nodes().collect()


# --- #48: traversal composition boundaries ---------------------------------
@native
def test_traversal_result_advertises_src_dst_columns():
    edges = _edges()
    frame = ur.hop(edges, 1).from_([0])
    got = frame.filter(ur.col("dst") > 0).collect()  # filter on the real column name
    assert set(got.columns) == {"src", "dst"}


@native
def test_hop_of_hop_raises_traversal_message():
    edges = _edges()
    inner = ur.hop(edges, 1).from_([0])
    with pytest.raises(NotImplementedError, match="traversal result"):
        ur.hop(inner, 1).from_([0]).collect()


@native
def test_seed_from_derived_nodeframe_raises_seed_message():
    edges = _edges()
    seed = edges.nodes().filter(ur.col("x") > 0)
    with pytest.raises(NotImplementedError, match="seed"):
        ur.hop(edges, 1).from_(seed).collect()
