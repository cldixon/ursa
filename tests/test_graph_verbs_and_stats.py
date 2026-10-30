"""Graph verbs and stats behave consistently (#213, #210).

1. A standalone graph verb (``ur.degree(e)``) is a full NodeFrame of
   ``(id, value)``: every frame method, ``schema()`` and ``explain()``. Inside
   ``with_columns`` it stays an expression.
2. ``density`` / ``avg_path_length`` / ``diameter`` are one-row frames
   (``lazy=True``; called as before they return the number) over the subgraph
   view: a filter's mask and a traversal's induced subgraph (#210).
3. A bare ``edges.nodes()`` collects to its node set.
4. ``describe(f, nodes="touched")`` describes only the nodes the view touches.
"""

import random

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa import col

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# #210's repro: 0->1 and 2->3 are light, 0->2 and 1->2 heavy.
TABLE = pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 2, 3], "w": [0.1, 0.9, 0.9, 0.1]})
HEAVY = col("w") > 0.5


def _edges():
    return ur.from_arrow(TABLE, src="s", dst="d")


def _random_table(n=60, m=180, seed=7):
    rng = random.Random(seed)
    s = [rng.randrange(n) for _ in range(m)]
    d = [rng.randrange(n) for _ in range(m)]
    w = [rng.random() for _ in range(m)]
    return pa.table({"s": s, "d": d, "w": w})


def _reingest(frame):
    return ur.from_arrow(frame.collect().to_arrow(), src=frame.src_col, dst=frame.dst_col)


def _bits(x):
    """A value with a float compared by its exact bits."""
    return x.hex() if isinstance(x, float) else x


# --- 1. a standalone verb is a frame -----------------------------------------


def _as_nodes(e):
    return e.nodes().with_columns(degree=ur.degree(e))


def test_a_standalone_verb_is_a_node_frame():
    e = _edges()
    d = ur.degree(e)
    assert isinstance(d, ur.NodeFrame)
    assert isinstance(d, ur.Expr)
    assert d.id_col == "id"
    assert d.collect().to_arrow().equals(_as_nodes(e).collect().to_arrow())
    assert d.schema().equals(_as_nodes(e).schema(), check_metadata=True)
    assert d.explain() == _as_nodes(e).explain()


def test_a_standalone_verb_has_every_frame_method():
    d = ur.degree(_edges())
    public = [n for n in dir(ur.NodeFrame) if not n.startswith("_")]
    assert {"join", "group_by", "rename", "sample", "limit", "schema", "explain"} <= set(public)
    for name in public:
        assert hasattr(d, name), name


def test_the_issue_repro_joins_and_sorts():
    # #213: `ur.degree(e).join(...)` raised AttributeError on main.
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    got = ur.degree(e).join(ur.pagerank(e), on="id").sort("pagerank", "id", descending=True)
    want = (
        e.nodes()
        .with_columns(degree=ur.degree(e), pagerank=ur.pagerank(e))
        .sort("pagerank", "id", descending=True)
    )
    assert got.collect().to_arrow().equals(want.collect().to_arrow())
    assert type(got) is ur.NodeFrame


@pytest.mark.parametrize(
    "step",
    [
        lambda f: f.filter(col("degree") > 1),
        lambda f: f.select("id"),
        lambda f: f.with_columns(x=col("degree") * 2),
        lambda f: f.rename({"degree": "dg"}),
        lambda f: f.sort("degree", "id"),
        lambda f: f.head(3),
        lambda f: f.limit(3),
        lambda f: f.sample(4, seed=3),
        lambda f: f.distinct(),
        lambda f: f.group_by("degree").agg(n=col("id").count()),
        lambda f: f.join(ur.from_arrow(pa.table({"id": [0, 2]}), id="id"), on="id", how="semi"),
    ],
)
def test_each_frame_method_matches_the_with_columns_spelling(step):
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    got = step(ur.degree(e))
    assert type(got) is ur.NodeFrame
    assert got.collect().to_arrow().equals(step(_as_nodes(e)).collect().to_arrow())


def test_a_standalone_verb_is_a_join_right_side_and_a_seed_set():
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    left = e.nodes().with_columns(c=ur.closeness(e))
    got = left.join(ur.degree(e), on="id").collect().to_arrow()
    assert got.column_names == ["id", "c", "degree"]
    want = ur.hop(e, 1).from_(_as_nodes(e).filter(col("degree") > 4))
    hop = ur.hop(e, 1).from_(ur.degree(e).filter(col("degree") > 4))
    assert hop.collect().to_arrow().equals(want.collect().to_arrow())


def test_inside_with_columns_a_verb_stays_an_expression():
    e = _edges()
    d = ur.degree(e)
    got = e.nodes().with_columns(d=d).collect().to_arrow()
    assert got.column("d").to_pylist() == [2, 1, 1, 0]
    # Used as an expression, the verb never built a frame of its own.
    assert "_node" not in vars(d)
    assert repr(d) == "degree(...)"
    assert isinstance(d == 1, ur.Expr)


def test_a_layout_is_a_frame_of_both_axes():
    from ursa._graph import layout_circle

    e = _edges()
    got = layout_circle(e).join(ur.degree(e), on="id").collect().to_arrow()
    assert got.column_names == ["id", "x", "y", "degree"]


# --- 2. lazy, masked density / diameter / avg_path_length (#210) -------------


def test_the_issue_210_repro():
    f = _edges().filter(HEAVY)
    assert ur.density(f) == pytest.approx(2 / 12)
    assert ur.diameter(f, approximate=False) == 1
    assert ur.avg_path_length(f) == 1.0


def test_the_eager_call_returns_a_plain_number():
    e = _edges()
    assert type(ur.density(e)) is float
    assert type(ur.avg_path_length(e)) is float
    assert type(ur.diameter(e)) is int


@pytest.mark.parametrize(
    "fn, column, dtype",
    [
        (lambda e: ur.density(e, lazy=True), "density", pa.float64()),
        (lambda e: ur.avg_path_length(e, lazy=True), "avg_path_length", pa.float64()),
        (lambda e: ur.diameter(e, approximate=False, lazy=True), "diameter", pa.int64()),
    ],
)
def test_lazy_stats_are_one_row_frames(fn, column, dtype):
    frame = fn(_edges().filter(HEAVY))
    assert isinstance(frame, ur.NodeFrame)
    table = frame.collect().to_arrow()
    assert table.schema == pa.schema([pa.field(column, dtype, nullable=False)])
    assert table.num_rows == 1
    assert frame.schema().equals(table.schema, check_metadata=True)
    assert f"{column}(" in frame.explain()
    # It composes like describe(): steps run on the row.
    doubled = frame.select(column, (col(column) * 2).alias("x")).collect().to_dicts()[0]
    assert doubled["x"] == 2 * table.column(0)[0].as_py()


def test_lazy_and_eager_agree_bit_for_bit():
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    for f in [e, e.filter(col("w") > 0.3), ur.hop(e, 2).from_([0, 5])]:
        for direction in ["out", "in", "both"]:
            for fn in [
                lambda f, **k: ur.density(f, **k),
                lambda f, **k: ur.avg_path_length(f, **k),
                lambda f, **k: ur.avg_path_length(f, sample=0.3, **k),
                lambda f, **k: ur.diameter(f, **k),
                lambda f, **k: ur.diameter(f, approximate=False, **k),
            ]:
                eager = fn(f, direction=direction)
                lazy = fn(f, direction=direction, lazy=True).collect().to_arrow().column(0)[0]
                assert _bits(eager) == _bits(lazy.as_py())


@pytest.mark.parametrize("direction", ["out", "in", "both"])
def test_path_stats_over_a_filter_match_a_reingest(direction):
    # Exact path stats read only reachable pairs, so the parent's untouched
    # nodes change nothing: the view and a re-ingest of its rows agree.
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    for threshold in [0.2, 0.5, 0.8, 0.97]:
        f = e.filter(col("w") > threshold)
        r = _reingest(f)
        assert ur.diameter(f, approximate=False, direction=direction) == ur.diameter(
            r, approximate=False, direction=direction
        )
        assert ur.avg_path_length(f, direction=direction) == pytest.approx(
            ur.avg_path_length(r, direction=direction), abs=1e-12
        )


@pytest.mark.parametrize("direction", ["out", "in", "both"])
def test_density_is_describe_density(direction):
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    for f in [e, e.filter(col("w") > 0.5), e.distinct().filter(col("w") < 0.4)]:
        row = ur.describe(f, direction=direction).collect().to_dicts()[0]
        assert _bits(ur.density(f, direction=direction)) == _bits(row["density"])
    # The view's density divides by the parent's n.
    f = _edges().filter(HEAVY)
    assert ur.density(f, direction="both") == pytest.approx(2 / 6)


def _induced(table, frame, seeds):
    """The rows of `table` among the nodes a traversal `frame` reached."""
    reached = pa.concat_arrays(
        [pa.array(seeds, pa.int64()), frame.collect().to_arrow().column("dst").combine_chunks()]
    )
    keep = pc.field("s").isin(reached) & pc.field("d").isin(reached)
    return ur.from_arrow(table.filter(keep), src="s", dst="d")


def test_stats_over_a_traversal_read_its_induced_subgraph():
    t = _random_table(n=80, m=120, seed=3)
    e = ur.from_arrow(t, src="s", dst="d")
    hop = ur.hop(e, 2).from_([1, 2])
    ref = _induced(t, hop, [1, 2])
    assert ur.describe(hop).collect().to_dicts()[0]["n_edges"] == ref.collect().to_arrow().num_rows
    for direction in ["out", "both"]:
        assert ur.diameter(hop, approximate=False, direction=direction) == ur.diameter(
            ref, approximate=False, direction=direction
        )
        assert ur.avg_path_length(hop, direction=direction) == pytest.approx(
            ur.avg_path_length(ref, direction=direction), abs=1e-12
        )
        row = ur.describe(hop, direction=direction).collect().to_dicts()[0]
        assert ur.density(hop, direction=direction) == row["density"]


def test_stats_over_a_scanned_filter(tmp_path):
    t = _random_table()
    pq.write_table(t, tmp_path / "e.parquet", row_group_size=40)
    scan = ur.scan_edges(str(tmp_path / "e.parquet"), src="s", dst="d").filter(col("w") > 0.5)
    mem = ur.from_arrow(t, src="s", dst="d").filter(col("w") > 0.5)
    for fn in [
        ur.density,
        ur.avg_path_length,
        lambda f: ur.diameter(f, approximate=False),
    ]:
        assert _bits(fn(scan)) == _bits(fn(mem))
    assert ur.density(scan) != ur.density(
        ur.scan_edges(str(tmp_path / "e.parquet"), src="s", dst="d")
    )


def test_an_estimate_over_a_small_view_samples_its_touched_nodes():
    # A 2,000-node path with a 3-edge view: the approximate diameter's 128
    # sources are drawn from the view's 4 nodes, so it is exact here.
    n = 2000
    t = pa.table({"s": list(range(n - 1)), "d": list(range(1, n))})
    e = ur.from_arrow(t, src="s", dst="d")
    f = e.filter((col("s") >= 1500) & (col("s") < 1503))
    assert ur.diameter(f) == 3
    assert ur.avg_path_length(f, sample=1.0) == pytest.approx(10 / 6)


def test_stats_over_the_whole_graph_are_unchanged():
    # The unmasked stats run the same kernels as before, on every core count.
    import networkx as nx

    t = _random_table(n=40, m=150, seed=11)
    e = ur.from_arrow(t, src="s", dst="d")
    g = nx.DiGraph()
    g.add_edges_from(zip(t["s"].to_pylist(), t["d"].to_pylist(), strict=True))
    assert ur.density(e.select("s", "d").distinct()) == pytest.approx(nx.density(g))
    u = g.to_undirected()
    u.remove_edges_from(nx.selfloop_edges(u))
    big = max(nx.connected_components(u), key=len)
    if len(big) == u.number_of_nodes():
        assert ur.diameter(e, approximate=False, direction="both") == nx.diameter(u)


# --- 3. a bare edges.nodes() is its node set ---------------------------------


@pytest.mark.parametrize("ids", ["int", "str"])
def test_bare_nodes_collect_is_the_node_set(ids):
    t = _random_table()
    if ids == "str":
        t = t.set_column(0, "s", pc.cast(t["s"], pa.string()))
        t = t.set_column(1, "d", pc.cast(t["d"], pa.string()))
    e = ur.from_arrow(t, src="s", dst="d")
    got = e.nodes().collect().to_arrow()
    want = _as_nodes(e).select("id").collect().to_arrow()
    assert got.equals(want)
    assert got.column_names == ["id"]
    assert e.nodes().schema().equals(got.schema, check_metadata=True)


def test_bare_nodes_takes_the_steps_after_it():
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    got = e.nodes().filter(col("id") > 10).sort("id", descending=True).head(3)
    want = _as_nodes(e).select("id").filter(col("id") > 10).sort("id", descending=True).head(3)
    assert got.collect().to_arrow().equals(want.collect().to_arrow())
    attrs = ur.from_arrow(pa.table({"id": [3, 4, 999], "v": [1, 2, 3]}), id="id")
    semi = attrs.join(e.nodes(), on="id", how="semi").collect().to_arrow()
    assert semi.column("id").to_pylist() == [
        i for i in [3, 4, 999] if i in set(e.nodes().collect().to_arrow()["id"].to_pylist())
    ]


def test_bare_nodes_reads_the_view_as_a_graph_column_does(tmp_path):
    e = _edges()
    # A filter is a subgraph view: the parent's node set.
    assert e.filter(HEAVY).nodes().collect().to_dicts() == [{"id": i} for i in [0, 1, 2, 3]]
    # A reshaping step makes the node set its rows' nodes.
    assert e.head(1).nodes().collect().to_dicts() == [{"id": 0}, {"id": 1}]
    assert (
        e.head(1)
        .nodes()
        .collect()
        .to_arrow()
        .equals(
            e.head(1).nodes().with_columns(d=ur.degree(e.head(1))).select("id").collect().to_arrow()
        )
    )
    pq.write_table(TABLE, tmp_path / "e.parquet")
    scan = ur.scan_edges(str(tmp_path / "e.parquet"), src="s", dst="d")
    assert scan.nodes().collect().to_arrow().equals(e.nodes().collect().to_arrow())


# --- 4. describe(nodes="touched") --------------------------------------------


def _row(frame, **kw):
    return ur.describe(frame, **kw).collect().to_dicts()[0]


def test_describe_touched_on_the_repro():
    f = _edges().filter(HEAVY)
    # Kept: 0->2 and 1->2, so the view touches 0, 1 and 2.
    assert _row(f, full=True, nodes="touched") == {
        "n_nodes": 3,
        "n_edges": 2,
        "density": pytest.approx(2 / 6),
        "avg_degree": pytest.approx(2 / 3),
        "n_components": 1,
    }
    # The default keeps the parent's node set (#208).
    assert _row(f, full=True)["n_nodes"] == 4
    assert _row(f, full=True) == _row(f, full=True, nodes="all")


@pytest.mark.parametrize("direction", ["out", "in", "both"])
def test_describe_touched_matches_a_reingest(direction):
    e = ur.from_arrow(_random_table(n=200, m=150, seed=5), src="s", dst="d")
    for threshold in [0.1, 0.6, 0.95]:
        f = e.filter(col("w") > threshold)
        got = _row(f, full=True, direction=direction, nodes="touched")
        want = _row(_reingest(f), full=True, direction=direction)
        assert got == want


def test_describe_touched_over_a_traversal():
    t = _random_table(n=80, m=120, seed=3)
    hop = ur.hop(ur.from_arrow(t, src="s", dst="d"), 2).from_([1, 2])
    ref = _induced(t, hop, [1, 2])
    assert _row(hop, full=True, nodes="touched") == _row(ref, full=True)


def test_describe_touched_over_the_whole_graph_changes_nothing():
    e = ur.from_arrow(_random_table(), src="s", dst="d")
    for direction in ["out", "both"]:
        assert _row(e, full=True, direction=direction, nodes="touched") == _row(
            e, full=True, direction=direction
        )


def test_describe_nodes_is_checked_where_it_is_called():
    with pytest.raises(ValueError, match=r"describe\(nodes='some'\) is not supported"):
        ur.describe(_edges(), nodes="some")
    with pytest.raises(ValueError, match=r"describe\(nodes=1\) is not supported"):
        ur.describe(_edges(), nodes=1)  # ty: ignore[invalid-argument-type]
    explained = ur.describe(_edges().filter(HEAVY), nodes="touched").explain()
    assert 'nodes="touched"' in explained
