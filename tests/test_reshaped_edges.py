"""Graph ops over reshaped edge sets (#191, part 3).

A graph algorithm or traversal over an edge frame after ``distinct``, ``sample``,
``join``, ``group_by().agg()``, ``sort`` or ``head`` runs over the topology of
that frame's rows. The reference is the re-ingest: collecting the reshaped edges
and building a fresh frame over them with ``ur.from_arrow(...)``. Every result
here equals it exactly: values (floats by their bits), node order and row order.
``filter`` above the reshaping step stays a subgraph view over its rows, and
``select`` / ``rename`` / ``with_columns`` keep the rows, so they keep the
topology too.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

EDGES = pa.table(
    {
        "s": [3, 0, 0, 1, 2, 3, 0, 4, 2, 5, 0, 1],
        "d": [1, 1, 2, 0, 0, 2, 1, 5, 3, 4, 2, 0],
        "w": [0.7, 0.1, 0.9, 0.6, 0.2, 0.8, 0.5, 1.5, 0.3, 2.0, 0.9, 0.4],
    }
)
ALLOWED = pa.table({"s": [0, 3, 4], "tag": ["a", "b", "c"]})


def _edges(source="memory", tmp_path=None):
    if source == "memory" or tmp_path is None:
        return ur.from_arrow(EDGES, src="s", dst="d")
    path = tmp_path / "e.parquet"
    pq.write_table(EDGES, path, row_group_size=5)
    return ur.scan_edges(str(path), src="s", dst="d")


def _reingest(frame):
    """The reference: the reshaped edges, collected and built into a new frame."""
    return ur.from_arrow(frame.collect().to_arrow(), src=frame.src_col, dst=frame.dst_col)


RESHAPES = {
    "distinct": lambda e: e.distinct(),
    "sample": lambda e: e.sample(7, seed=3),
    "semi_join": lambda e: e.join(ur.from_arrow(ALLOWED, id="s"), on="s", how="semi"),
    "inner_join": lambda e: e.join(ur.from_arrow(ALLOWED, id="s"), on="s"),
    "group_by": lambda e: e.group_by("s", "d").agg(w=ur.col("w").sum()),
    "sort": lambda e: e.sort("w", descending=True),
    "head": lambda e: e.head(6),
    "filter_then_head": lambda e: e.filter(ur.col("w") > 0.25).head(5),
    "head_then_filter": lambda e: e.head(8).filter(ur.col("w") > 0.25),
    "distinct_then_select": lambda e: e.distinct().select("s", "d", "w"),
    "reverse_after": lambda e: e.distinct().reverse(),
    "reverse_before": lambda e: e.reverse().head(9),
    "with_columns": lambda e: e.with_columns(w2=ur.col("w") * 2).head(10),
}

ALGOS = {
    "degree": lambda e: ur.degree(e),
    "degree_in": lambda e: ur.degree(e, "in"),
    "pagerank_w": lambda e: ur.pagerank(e, weight=ur.col("w")),
    "pagerank_both": lambda e: ur.pagerank(e, direction="both"),
    "components": lambda e: ur.connected_components(e),
    "triangles": lambda e: ur.triangle_count(e),
    "louvain": lambda e: ur.louvain(e, seed=7),
    "betweenness": lambda e: ur.betweenness(e, sample=0.5, seed=1),
    "closeness": lambda e: ur.closeness(e),
    "hop": lambda e: ur.hop(e, 2).from_([0, 4]),
    "shortest_path": lambda e: ur.shortest_path(e, 3, 2),
    "random_walk": lambda e: ur.random_walk(e, start=[0, 1], steps=4, seed=5),
}


def _same(got, want):
    assert got.schema == want.schema
    assert got.equals(want), f"\n{got.to_pydict()}\n!=\n{want.to_pydict()}"


@pytest.mark.parametrize("source", ["memory", "scan"])
@pytest.mark.parametrize("reshape", sorted(RESHAPES))
@pytest.mark.parametrize("algo", sorted(ALGOS))
def test_matches_the_reingested_frame(source, reshape, algo, tmp_path):
    reshaped = RESHAPES[reshape](_edges(source, tmp_path))
    run = ALGOS[algo]
    want = run(_reingest(reshaped)).collect().to_arrow()
    _same(run(reshaped).collect().to_arrow(), want)


def test_the_examples_from_the_issue():
    e = _edges()
    grouped = e.group_by("s", "d").agg(w=ur.col("w").sum())
    _same(
        ur.pagerank(grouped, weight=ur.col("w")).collect().to_arrow(),
        ur.pagerank(_reingest(grouped), weight=ur.col("w")).collect().to_arrow(),
    )
    semi = e.join(ur.from_arrow(ALLOWED, id="s"), on="s", how="semi")
    _same(ur.degree(semi).collect().to_arrow(), ur.degree(_reingest(semi)).collect().to_arrow())
    hop = ur.hop(e.distinct(), 2).from_([0])
    want = ur.hop(_reingest(e.distinct()), 2).from_([0]).collect().to_arrow()
    _same(hop.collect().to_arrow(), want)


def test_a_filter_above_the_reshape_is_a_view_over_its_rows():
    # The filter masks the reshaped topology: every node of the distinct edges
    # stays, as a filter over a source frame keeps every node.
    e = _edges()
    view = e.distinct().filter(ur.col("w") > 0.5)
    want = ur.degree(_reingest(e.distinct()).filter(ur.col("w") > 0.5)).collect().to_arrow()
    _same(ur.degree(view).collect().to_arrow(), want)
    assert want.num_rows == ur.degree(e.distinct()).collect().to_arrow().num_rows


def test_a_graph_op_over_a_traversal_of_a_reshaped_frame():
    e = _edges()
    got = ur.degree(ur.hop(e.sample(8, seed=2), 1).from_([0])).collect().to_arrow()
    want = ur.degree(ur.hop(_reingest(e.sample(8, seed=2)), 1).from_([0])).collect().to_arrow()
    _same(got, want)


def test_a_weight_from_an_aggregate_or_a_computed_column():
    e = _edges()
    computed = e.with_columns(w2=ur.col("w") * 3)
    _same(
        ur.pagerank(computed, weight=ur.col("w2")).collect().to_arrow(),
        ur.pagerank(_reingest(computed), weight=ur.col("w2")).collect().to_arrow(),
    )
    renamed = e.rename({"w": "cost"})
    _same(
        ur.closeness(renamed, weight=ur.col("cost")).collect().to_arrow(),
        ur.closeness(e, weight=ur.col("w")).collect().to_arrow(),
    )


def test_column_steps_keep_the_parent_index_and_scope_its_caches():
    e = _edges()
    ur.pagerank(e, weight=ur.col("w")).collect()
    doubled = e.with_columns(w=ur.col("w") * 2)
    # Same topology, different weights: the cached weights of `w` are not reused.
    got = ur.pagerank(doubled, weight=ur.col("w")).collect().to_arrow()
    want = ur.pagerank(_reingest(doubled), weight=ur.col("w")).collect().to_arrow()
    _same(got, want)
    # A filter after a computed column reads it, and gets its own mask.
    view = doubled.filter(ur.col("w") > 1.0)
    _same(
        ur.degree(view).collect().to_arrow(),
        ur.degree(_reingest(doubled).filter(ur.col("w") > 1.0)).collect().to_arrow(),
    )
    _same(
        ur.degree(e.filter(ur.col("w") > 1.0)).collect().to_arrow(),
        ur.degree(_reingest(e).filter(ur.col("w") > 1.0)).collect().to_arrow(),
    )


@pytest.mark.parametrize(
    ("make", "message"),
    [
        (lambda e: e.select("s", "w"), r"dst column 'd', but select\(\) removed it"),
        (
            lambda e: e.rename({"s": "a"}),
            r"src column 's', but rename\(\) renamed it to 'a'; a rename does not move",
        ),
        (
            lambda e: e.group_by("s").agg(n=ur.col("d").count()),
            r"dst column 'd', but group_by\(\).agg\(\) removed it",
        ),
        (lambda e: e.distinct().select("d"), r"src column 's', but select\(\) removed it"),
    ],
)
def test_a_step_that_drops_a_role_column_errors(make, message):
    e = _edges()
    with pytest.raises(ur.ColumnNotFoundError, match=message):
        ur.degree(make(e)).collect()


def test_a_role_column_can_come_back():
    e = _edges()
    back = e.rename({"s": "a"}).rename({"a": "s"})
    _same(ur.degree(back).collect().to_arrow(), ur.degree(e).collect().to_arrow())


def test_edge_frame_with_columns_collects():
    got = _edges().with_columns(w2=ur.col("w") * 2, s2=ur.src() + 1).collect().to_arrow()
    assert got.column_names == ["s", "d", "w", "w2", "s2"]
    assert got.column("w2").to_pylist() == [w * 2 for w in EDGES.column("w").to_pylist()]
    with pytest.raises(NotImplementedError, match="'with_columns' step on an edge frame"):
        e = _edges()
        e.with_columns(d2=ur.degree(e)).collect()


# --- several edge frames in one with_columns ----------------------------------


NODES = pa.table({"id": [5, 0, 1, 9, 2, 3, 4], "name": list("fabzcde")})


def test_mixed_edge_frames_in_one_with_columns():
    e = _edges()
    view = e.filter(ur.col("w") > 0.5)
    nodes = ur.from_arrow(NODES, id="id")
    got = nodes.with_columns(
        pr=ur.pagerank(e),
        d2=ur.degree(view),
        d3=ur.degree(e.distinct()),
        k=ur.col("id") * 10,
    )
    # The same columns, one edge frame per collect, attached by id.
    want = (
        nodes.with_columns(pr=ur.pagerank(e))
        .with_columns(d2=ur.degree(view))
        .with_columns(d3=ur.degree(e.distinct()), k=ur.col("id") * 10)
    )
    _same(got.collect().to_arrow(), want.collect().to_arrow())
    out = got.collect().to_arrow()
    assert out.column_names == ["id", "name", "pr", "d2", "d3", "k"]
    # Node 9 is in no graph: its graph columns are null, as an attach makes them.
    row = out.to_pylist()[3]
    assert row["id"] == 9 and row["pr"] is None and row["d3"] is None


def test_mixed_edge_frames_over_the_node_set():
    # On `edges.nodes()` the first graph column's edges decide the node set; the
    # others attach by id, null where their graph lacks the node.
    e = _edges()
    small = e.head(3)
    got = e.nodes().with_columns(d=ur.degree(e), d_small=ur.degree(small)).collect().to_arrow()
    want = (
        e.nodes()
        .with_columns(d=ur.degree(e))
        .with_columns(d_small=ur.degree(small))
        .collect()
        .to_arrow()
    )
    _same(got, want)
    assert got.column("d_small").null_count > 0


def test_nodes_of_a_reshaped_frame():
    # `X.nodes()` with X reshaped: X's rows give the node set; a column over other
    # edges attaches to it by id.
    e = _edges()
    small = e.head(3)
    got = small.nodes().with_columns(d=ur.degree(e)).collect().to_arrow()
    nodes_of_small = ur.degree(small).collect().to_arrow().column("id")
    assert got.column("id").equals(nodes_of_small)
    full = {r["id"]: r["degree"] for r in ur.degree(e).collect().to_dicts()}
    assert got.column("d").to_pylist() == [full[i] for i in nodes_of_small.to_pylist()]
    # It is what `ur.degree(small)` itself runs.
    _same(
        small.nodes().with_columns(degree=ur.degree(small)).collect().to_arrow(),
        ur.degree(_reingest(small)).collect().to_arrow(),
    )


def test_a_step_on_the_edges_of_a_traversal_before_nodes_still_errors():
    e = _edges()
    hop = ur.hop(e, 1).from_([0])
    with pytest.raises(NotImplementedError, match="relational step"):
        hop.head(2).nodes().with_columns(d=ur.degree(hop.head(2))).collect()


# --- main bugs: a step on the edges a graph op silently ignored ---------------


def test_a_traversal_or_describe_no_longer_ignores_head_or_sort():
    # Before #191 part 3 these ran over the whole graph, as if `head` / `sort`
    # were not there: the walk took edges `head` drops.
    e = _edges()
    small = e.head(3)
    walk = ur.random_walk(small, start=[0, 1, 3], steps=6, seed=4).collect().to_arrow()
    kept = {
        (s, d) for s, d in zip(EDGES["s"][:3].to_pylist(), EDGES["d"][:3].to_pylist(), strict=True)
    }
    nodes = walk.column("node").to_pylist()
    walks = walk.column("walk_id").to_pylist()
    for i in range(1, len(nodes)):
        if walks[i] == walks[i - 1]:
            assert (nodes[i - 1], nodes[i]) in kept
    for frame in (small, e.sort("w")):
        for run in (
            lambda f: ur.describe(f),
            lambda f: ur.hop(f, 2).from_([0, 3]),
            lambda f: ur.shortest_path(f, 3, 2),
            lambda f: ur.from_arrow(NODES, id="id").with_columns(d=ur.degree(f)),
        ):
            _same(run(frame).collect().to_arrow(), run(_reingest(frame)).collect().to_arrow())


def test_a_traversal_weight_reads_the_column_with_columns_replaced():
    # The weight used to read the source's `w`, not the replaced one.
    e = _edges()
    heavy = e.with_columns(w=ur.col("w") * 100 + ur.col("s"))
    got = ur.shortest_path(heavy, 3, 2, weight=ur.col("w")).collect().to_arrow()
    want = ur.shortest_path(_reingest(heavy), 3, 2, weight=ur.col("w")).collect().to_arrow()
    _same(got, want)
    # A column select() dropped is gone for the weight too (it used to be read
    # from the source).
    with pytest.raises(ur.ColumnNotFoundError, match="unknown edge column"):
        ur.shortest_path(e.select("s", "d"), 3, 2, weight=ur.col("w")).collect()
