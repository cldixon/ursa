"""Relational steps take effect in the order written (#162, #164).

Polars — whose surface ursa follows — applies `select`/`distinct`/`rename`/
`sort`/`head`/`filter`/`sample`/`group_by` in the order they are written. These
tests pin that on every collect path: plain in-memory and scanned node/edge
frames (the pyarrow tail, with scan projection pushdown), graph-derived node
frames (the DataFusion engine tail plus the out-of-order remainder), traversal
results, and joins. Expected values are exact and cross-checked against pyarrow
computing the same steps in the same order.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa._execute import _split_engine_tail

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

NODES = pa.table(
    {"id": [1, 2, 3, 4, 5, 6], "c": ["x", "y", "x", "y", "x", "z"], "v": [3, 1, 2, 5, 4, 6]}
)
EDGES = pa.table(
    {
        "s": [1, 2, 3, 4, 5, 6],
        "d": [2, 3, 1, 1, 1, 2],
        "c": ["x", "y", "x", "y", "x", "z"],
        "v": [3, 1, 2, 5, 4, 6],
    }
)


@pytest.fixture(params=["memory", "scan"])
def nodes(request, tmp_path):
    if request.param == "memory":
        return ur.from_arrow(NODES, id="id")
    path = tmp_path / "nodes.parquet"
    pq.write_table(NODES, path)
    return ur.scan_nodes(str(path), id="id")


@pytest.fixture(params=["memory", "scan"])
def edges(request, tmp_path):
    if request.param == "memory":
        return ur.from_arrow(EDGES, src="s", dst="d")
    path = tmp_path / "edges.parquet"
    pq.write_table(EDGES, path)
    return ur.scan_edges(str(path), src="s", dst="d")


def _distinct(table: pa.Table) -> pa.Table:
    return table.group_by(table.column_names).aggregate([])


def _sorted_rows(rows: list[dict]) -> list[dict]:
    return sorted(rows, key=lambda r: tuple(str(v) for v in r.values()))


# --- #162: select -> distinct ------------------------------------------------


def test_select_then_distinct_dedups_the_selected_columns(nodes):
    out = nodes.select("c").distinct().sort("c").collect().to_dicts()
    ref = _distinct(NODES.select(["c"])).sort_by("c").to_pylist()
    assert out == ref == [{"c": "x"}, {"c": "y"}, {"c": "z"}]


def test_distinct_then_select_keeps_one_row_per_distinct_full_row(nodes):
    # Every full row is distinct, so all six survive the distinct; select then
    # narrows them (duplicates of c included).
    out = nodes.distinct().select("c").collect().to_dicts()
    assert _sorted_rows(out) == _sorted_rows(NODES.select(["c"]).to_pylist())
    assert len(out) == 6


def test_select_then_distinct_on_edges(edges):
    out = edges.select("c").distinct().sort("c").collect().to_dicts()
    assert out == [{"c": "x"}, {"c": "y"}, {"c": "z"}]
    out = edges.select("d").distinct().sort("d").collect().to_dicts()
    assert out == _distinct(EDGES.select(["d"])).sort_by("d").to_pylist()
    assert out == [{"d": 1}, {"d": 2}, {"d": 3}]


def test_select_then_distinct_on_graph_derived_frame(tmp_path):
    g = ur.from_arrow(EDGES, src="s", dst="d")
    path = tmp_path / "attrs.parquet"
    pq.write_table(NODES.select(["id", "c"]), path)
    in_memory = ur.from_arrow(NODES.select(["id", "c"]), id="id")
    for attrs in (in_memory, ur.scan_nodes(str(path), id="id")):
        frame = attrs.with_columns(deg=ur.degree(g, direction="in"))
        assert frame.select("c").distinct().sort("c").collect().to_dicts() == [
            {"c": "x"},
            {"c": "y"},
            {"c": "z"},
        ]
        # in-degrees: 1 <- 3, 4, 5; 2 <- 1, 6; 3 <- 2; the rest 0 -> {0, 1, 2, 3}.
        assert frame.select("deg").distinct().sort("deg").collect().to_dicts() == [
            {"deg": 0},
            {"deg": 1},
            {"deg": 2},
            {"deg": 3},
        ]


# --- #164: rename, then steps that use the new name -------------------------


def test_rename_then_sort_by_new_name(nodes):
    out = nodes.rename({"v": "val"}).sort("val").collect().to_arrow()
    assert out.column_names == ["id", "c", "val"]
    assert out.column("val").to_pylist() == [1, 2, 3, 4, 5, 6]
    assert out.column("id").to_pylist() == [2, 3, 1, 5, 4, 6]


def test_rename_then_filter_select_on_new_name(nodes):
    out = (
        nodes.rename({"v": "val"})
        .filter(ur.col("val") > 3)
        .select("id", "val")
        .sort("id")
        .collect()
        .to_dicts()
    )
    greater = pc.greater  # ty: ignore[unresolved-attribute]
    ref = NODES.filter(greater(NODES.column("v"), 3)).select(["id", "v"]).sort_by("id")
    assert out == ref.rename_columns(["id", "val"]).to_pylist()
    assert out == [{"id": 4, "val": 5}, {"id": 5, "val": 4}, {"id": 6, "val": 6}]


def test_filter_on_the_old_name_after_rename_errors(nodes):
    # The old name no longer exists after the rename: an error, not a silent match.
    with pytest.raises(Exception, match="'v'"):
        nodes.rename({"v": "val"}).filter(ur.col("v") > 3).collect()


def test_rename_then_group_by_on_new_key(nodes):
    out = nodes.rename({"c": "k"}).group_by("k").agg(total=ur.col("v").sum()).sort("k").collect()
    assert out.to_dicts() == [
        {"k": "x", "total": 9},
        {"k": "y", "total": 6},
        {"k": "z", "total": 6},
    ]


def test_rename_chain_and_swap(nodes):
    chained = nodes.rename({"v": "a"}).rename({"a": "b"}).sort("b").collect().to_arrow()
    assert chained.column_names == ["id", "c", "b"]
    assert chained.column("b").to_pylist() == [1, 2, 3, 4, 5, 6]
    swapped = nodes.rename({"v": "c", "c": "v"}).sort("c").collect().to_arrow()
    assert swapped.column_names == ["id", "v", "c"]
    assert swapped.column("c").to_pylist() == [1, 2, 3, 4, 5, 6]
    assert swapped.column("v").to_pylist() == ["y", "x", "x", "x", "y", "z"]


def test_select_then_rename_then_sort(nodes):
    out = nodes.select("c", "v").rename({"v": "w"}).sort("w", descending=True).head(2)
    assert out.collect().to_dicts() == [{"c": "z", "w": 6}, {"c": "y", "w": 5}]


def test_filter_after_select_cannot_see_dropped_column(nodes):
    with pytest.raises(Exception, match="'v'"):
        nodes.select("c").filter(ur.col("v") > 1).collect()


def test_rename_on_edges_then_sort(edges):
    out = edges.rename({"v": "val"}).sort("val", descending=True).head(2).collect().to_dicts()
    assert out == [{"s": 6, "d": 2, "c": "z", "val": 6}, {"s": 4, "d": 1, "c": "y", "val": 5}]


def test_rename_then_steps_on_graph_derived_frame():
    g = ur.from_arrow(EDGES, src="s", dst="d")
    frame = g.nodes().with_columns(deg=ur.degree(g, direction="in"))
    out = (
        frame.rename({"deg": "indeg"})
        .filter(ur.col("indeg") > 0)
        .sort("indeg", descending=True)
        .collect()
        .to_dicts()
    )
    assert out == [{"id": 1, "indeg": 3}, {"id": 2, "indeg": 2}, {"id": 3, "indeg": 1}]


# --- sort / head / filter / sample placement --------------------------------


def test_head_then_sort_sorts_only_the_head(nodes):
    out = nodes.head(3).sort("v", descending=True).collect().to_dicts()
    ref = _sort_desc(NODES.slice(0, 3), "v").to_pylist()
    assert out == ref
    assert [r["id"] for r in out] == [1, 3, 2]
    # The other order: the top three overall.
    top = nodes.sort("v", descending=True).head(3).collect().to_dicts()
    assert [r["id"] for r in top] == [6, 4, 5]


def _sort_desc(table: pa.Table, col: str) -> pa.Table:
    return table.sort_by([(col, "descending")])


def test_head_then_filter_filters_only_the_head(nodes, edges):
    # Rows by v ascending: v = 1, 2, 3 | 4, 5, 6. head(3) then v > 2 keeps one row.
    assert nodes.sort("v").head(3).filter(ur.col("v") > 2).collect().to_dicts() == [
        {"id": 1, "c": "x", "v": 3}
    ]
    assert nodes.sort("v").filter(ur.col("v") > 2).head(2).collect().to_dicts() == [
        {"id": 1, "c": "x", "v": 3},
        {"id": 5, "c": "x", "v": 4},
    ]
    assert edges.sort("v").head(3).filter(ur.col("v") > 2).collect().to_dicts() == [
        {"s": 1, "d": 2, "c": "x", "v": 3}
    ]


def test_head_then_distinct(nodes):
    # The first three rows by v carry c = y, x, x -> two distinct values.
    out = nodes.sort("v").head(3).select("c").distinct().sort("c").collect().to_dicts()
    assert out == [{"c": "x"}, {"c": "y"}]


def test_sample_placement_follows_written_order(nodes):
    # A plain frame samples positions with a seeded stdlib PRNG over the rows it is
    # given; head before sample hands it four rows, sample before head hands it six.
    import random

    head_then = nodes.sort("id").head(4).sample(2, seed=3).collect().to_dicts()
    idx = sorted(random.Random(3).sample(range(4), 2))
    assert head_then == [NODES.slice(0, 4).to_pylist()[i] for i in idx]
    sample_then = nodes.sort("id").sample(4, seed=3).head(2).collect().to_dicts()
    idx = sorted(random.Random(3).sample(range(6), 4))[:2]
    assert sample_then == [NODES.to_pylist()[i] for i in idx]


def test_head_after_head_and_sort_after_sort(nodes):
    assert [r["id"] for r in nodes.sort("id").head(4).head(2).collect().to_dicts()] == [1, 2]
    # The last sort decides the order (the first is not silently dropped: its
    # column must still exist).
    assert [r["v"] for r in nodes.sort("c").sort("v").collect().to_dicts()] == [1, 2, 3, 4, 5, 6]


def test_graph_derived_head_then_filter_and_sort():
    g = ur.from_arrow(EDGES, src="s", dst="d")
    frame = g.nodes().with_columns(deg=ur.degree(g, direction="in"))
    # sort by id: in-degrees 3, 2, 1, 0, 0, 0. head(4) then deg > 1 -> ids 1, 2.
    out = frame.sort("id").head(4).filter(ur.col("deg") > 1).collect().to_dicts()
    assert out == [{"id": 1, "deg": 3}, {"id": 2, "deg": 2}]
    # head(3) then sort ascending: the head is ids 1..3, re-sorted by deg.
    out = frame.sort("id").head(3).sort("deg").collect().to_dicts()
    assert out == [{"id": 3, "deg": 1}, {"id": 2, "deg": 2}, {"id": 1, "deg": 3}]
    # A sort the engine replaces must still validate its column.
    with pytest.raises(Exception, match="nope"):
        frame.sort("nope").sort("deg").collect()


def test_traversal_head_then_sort():
    g = ur.from_arrow(pa.table({"s": [0, 0, 0], "d": [3, 1, 2]}), src="s", dst="d")
    hop = ur.hop(g, 1).from_([0])
    # head(2) then sort by dst: two reached nodes, ordered; sort-then-head would
    # always give 1, 2.
    out = hop.sort("dst", descending=True).head(2).sort("dst").collect().to_dicts()
    assert out == [{"src": 0, "dst": 2}, {"src": 0, "dst": 3}]
    renamed = hop.rename({"dst": "to"}).sort("to").collect().to_dicts()
    assert [r["to"] for r in renamed] == [1, 2, 3]


# --- joins -------------------------------------------------------------------


def test_join_then_rename_then_sort_on_graph_derived_frame():
    g = ur.from_arrow(EDGES, src="s", dst="d")
    frame = g.nodes().with_columns(deg=ur.degree(g, direction="in"))
    other = ur.from_arrow(pa.table({"id": [1, 2, 3], "w": [10, 20, 30]}), id="id")
    out = frame.join(other, on="id").rename({"w": "z"}).sort("z", descending=True)
    assert out.collect().to_dicts() == [
        {"id": 3, "deg": 1, "z": 30},
        {"id": 2, "deg": 2, "z": 20},
        {"id": 1, "deg": 3, "z": 10},
    ]


def test_rename_then_join_on_edges(edges):
    names = ur.from_arrow(pa.table({"src": [1, 2], "name": ["a", "b"]}), id="src")
    out = edges.rename({"s": "src"}).join(names, on="src").sort("src").select("src", "name")
    assert out.collect().to_dicts() == [{"src": 1, "name": "a"}, {"src": 2, "name": "b"}]


# --- engine path: the common tail stays fully pushed down --------------------


def _steps(*kinds):
    return [(k, a) for k, a in kinds]


def test_engine_split_pushes_the_common_tail():
    sort = ("v", True)
    pushed, rest = _split_engine_tail(
        _steps(("filter", "f1"), ("sort", sort), ("filter", "f2"), ("head", 5))
    )
    assert rest == []
    assert pushed.filters == ["f1", "f2"] and pushed.sort == sort and pushed.limit == 5


@pytest.mark.parametrize(
    ("steps", "n_pushed"),
    [
        ([("head", 3), ("sort", ("v", False))], 1),
        ([("head", 3), ("filter", "f")], 1),
        ([("rename", {"v": "w"}), ("sort", ("w", False))], 1),
        ([("select", ["c"]), ("distinct", None)], 1),
        ([("sort", ("v", False)), ("sort", ("c", False))], 1),
        ([("rename", {"a": "b", "b": "a"})], 0),
        ([("group_by", ([], [], [])), ("filter", "f")], 1),
    ],
)
def test_engine_split_leaves_out_of_order_steps_to_the_remainder(steps, n_pushed):
    _pushed, rest = _split_engine_tail(steps)
    assert rest == steps[n_pushed:]


# --- clear errors where an ordering is not supported -------------------------


def test_with_columns_after_a_relational_step_is_rejected(nodes):
    g = ur.from_arrow(EDGES, src="s", dst="d")
    with pytest.raises(NotImplementedError, match=r"with_columns\(\) after head\(\)"):
        nodes.head(2).with_columns(deg=ur.degree(g)).collect()
    with pytest.raises(NotImplementedError, match=r"computes"):
        nodes.filter(ur.col("deg") > 0).with_columns(deg=ur.degree(g)).collect()
    # A filter on an attribute commutes with the computed columns.
    out = nodes.filter(ur.col("v") > 4).with_columns(deg=ur.degree(g, direction="in"))
    assert _sorted_rows(out.collect().to_dicts()) == [
        {"id": 4, "c": "y", "v": 5, "deg": 0},
        {"id": 6, "c": "z", "v": 6, "deg": 0},
    ]


def test_relational_step_on_edges_before_nodes_is_rejected():
    g = ur.from_arrow(EDGES, src="s", dst="d")
    with pytest.raises(NotImplementedError, match=r"head\(\) on the edges before nodes\(\)"):
        g.head(2).nodes().with_columns(deg=ur.degree(g)).collect()
