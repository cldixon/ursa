"""describe() over a filtered edge frame reads the subgraph view (#207).

A ``filter`` on an edge frame is a subgraph view (#114): the parent's node set,
and only the edge rows the predicate keeps. ``degree`` and every other graph op
read it that way; ``describe`` used to report the parent graph instead. A step
that reshapes the rows (``distinct``, ``head``, ...) runs over the topology of its
output rows, as a re-ingest of them would (#191), and a filter above it is a view
over that topology.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# The issue's repro: 0->1 and 2->3 are light, 0->2 and 1->2 heavy.
TABLE = pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 2, 3], "w": [0.1, 0.9, 0.9, 0.1]})
HEAVY = ur.col("w") > 0.5


def _edges():
    return ur.from_arrow(TABLE, src="s", dst="d")


def _row(frame, **kw):
    return ur.describe(frame, **kw).collect().to_dicts()[0]


def _degrees(frame, direction="out"):
    rows = frame.nodes().with_columns(dg=ur.degree(frame, direction=direction)).collect()
    return [r["dg"] for r in rows.to_dicts()]


def _degree_sum(frame, direction="out"):
    return sum(_degrees(frame, direction))


def _by_hand(n_nodes, n_edges, n_components=None, endpoints=1):
    density = n_edges / (n_nodes * (n_nodes - 1)) * endpoints if n_nodes > 1 else 0.0
    return {
        "n_nodes": n_nodes,
        "n_edges": n_edges,
        "density": pytest.approx(density),
        "avg_degree": pytest.approx(endpoints * n_edges / n_nodes),
        "n_components": n_components,
    }


def test_describe_of_a_filter_counts_only_the_kept_edges():
    # The view keeps the parent's 4 nodes (node 3 isolated) and 2 of its 4 edges.
    assert _row(_edges()) == _by_hand(4, 4)
    assert _row(_edges().filter(HEAVY)) == _by_hand(4, 2)


def test_describe_of_a_filter_computes_components_over_the_kept_edges():
    # Kept: 0->2, 1->2. Weakly: {0, 1, 2} and the isolated {3}.
    assert _row(_edges(), full=True)["n_components"] == 1
    assert _row(_edges().filter(HEAVY), full=True) == _by_hand(4, 2, n_components=2)


def test_describe_of_a_filter_undirected():
    # A reciprocal pair and a self-loop among the kept rows collapse / drop, as
    # they do over the whole graph.
    e = ur.from_arrow(
        pa.table({"s": [0, 1, 1, 2, 3], "d": [1, 0, 2, 2, 0], "k": [1, 1, 1, 1, 0]}),
        src="s",
        dst="d",
    )
    row = _row(e.filter(ur.col("k") == 1), full=True, direction="both")
    # Kept rows 0->1, 1->0, 1->2, 2->2: undirected pairs {0,1}, {1,2}.
    assert row == _by_hand(4, 2, n_components=2, endpoints=2)


@pytest.mark.parametrize("direction", ["out", "in"])
def test_describe_of_a_filter_agrees_with_degree(direction):
    f = _edges().filter(HEAVY)
    row = _row(f, direction=direction)
    assert row["n_edges"] == _degree_sum(f, direction)
    assert row["n_nodes"] == len(_degrees(f, direction))


def test_two_filters_intersect():
    f = _edges().filter(HEAVY).filter(ur.col("s") == 0)
    assert _row(f) == _by_hand(4, 1)
    assert _row(f)["n_edges"] == _degree_sum(f)


def test_a_filter_keeping_every_row_is_the_whole_graph():
    e = _edges()
    whole = ur.describe(e, full=True).collect().to_arrow()
    assert ur.describe(e.filter(ur.col("w") > 0), full=True).collect().to_arrow().equals(whole)


def test_describe_of_a_filtered_scan(tmp_path):
    path = tmp_path / "edges.parquet"
    pq.write_table(TABLE, path)
    scan = ur.scan_edges(str(path), src="s", dst="d")
    f = scan.filter(HEAVY)
    assert _row(f, full=True) == _row(_edges().filter(HEAVY), full=True)
    assert _row(f) == _by_hand(4, 2)
    assert _row(f)["n_edges"] == _degree_sum(f)


def test_a_filter_above_a_reshape_is_a_view_of_the_reshaped_rows():
    # distinct() drops the repeated 0->1 row; its topology is its rows' (4 nodes,
    # 3 edges), and the filter above it masks those to 0->1 and 2->3.
    e = ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2], "d": [1, 1, 2, 3], "w": [0.9, 0.9, 0.1, 0.9]}),
        src="s",
        dst="d",
    )
    assert _row(e.distinct(), full=True) == _by_hand(4, 3, n_components=1)
    f = e.distinct().filter(HEAVY)
    assert _row(f, full=True) == _by_hand(4, 2, n_components=2)
    assert _row(f)["n_edges"] == _degree_sum(f)


def test_a_filter_below_a_reshape_is_re_ingested():
    # The reshape runs over the filtered rows, as a fresh frame over them would:
    # node 3 (only on a dropped row) is not in that graph.
    e = _edges()
    f = e.filter(HEAVY).distinct()
    fresh = ur.from_arrow(f.collect().to_arrow(), src="s", dst="d")
    assert _row(f, full=True) == _row(fresh, full=True) == _by_hand(3, 2, n_components=1)


def test_describe_of_a_traversal_result_is_its_induced_view():
    # A traversal result is a view too (#116): the parent's nodes, and the edges
    # its reached set induces. hop 1 from node 1 reaches {1, 2}: the edge 1->2.
    e = ur.from_arrow(pa.table({"s": [0, 0, 1, 2, 4], "d": [1, 2, 2, 3, 5]}), src="s", dst="d")
    h = ur.hop(e, 1).from_([1])
    assert _row(h, full=True) == _by_hand(6, 1, n_components=5)
    assert _row(h)["n_edges"] == _degree_sum(h) == h.collect().to_arrow().num_rows
    # Over a filtered input, the induced subgraph stays within the filter.
    hf = ur.hop(_edges().filter(HEAVY), 2).from_([0])
    assert _row(hf) == _by_hand(4, 1)
    assert _row(hf)["n_edges"] == _degree_sum(hf)
    sp = ur.shortest_path(e, 0, 3)
    assert _row(sp)["n_edges"] == _degree_sum(sp) == 2
