"""filter() predicate algebra (issue #19).

Filters now lower through the same expression seam as ``weight=`` (Rust
``ursa_plan::expr``) instead of a flat ``col <op> number`` special case, so the
full predicate algebra is available: comparisons, boolean combinators
(``& | ~``), arithmetic inside predicates, ``col <op> col``, string/bool
equality, and the role references ``ur.src()`` / ``ur.dst()`` / ``ur.id()``.

Each predicate is exercised in **both** execution contexts that share the
dialect: a graph-op pipeline (lowered to DataFusion in Rust) and a plain-frame
collect (evaluated over pyarrow in Python). The two must agree.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# hub graph: 1,2,3 -> 0, and 0 -> 1.
#   in-degree:  0->3, 1->1, 2->0, 3->0
#   out-degree: every node -> 1
EDGES = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")


def _nodes_file(tmp_path) -> str:
    path = tmp_path / "nodes.csv"
    path.write_text("id,region,capacity\n0,us,10\n1,us,20\n2,eu,30\n3,eu,40\n")
    return str(path)


def _ids(df) -> set:
    return set(df.to_arrow().column("id").to_pylist())


# --- graph-op context (lowered to DataFusion) ------------------------------


def test_comparison_on_computed_column(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = (
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
        .filter(ur.col("indeg") >= 1)
        .collect()
    )
    assert _ids(df) == {0, 1}


def test_string_equality_on_attribute(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = (
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
        .filter(ur.col("region") == "us")
        .collect()
    )
    assert _ids(df) == {0, 1}


def test_col_op_col(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = (
        nodes.with_columns(
            indeg=ur.degree(EDGES, direction="in"),
            outdeg=ur.degree(EDGES, direction="out"),
        )
        .filter(ur.col("indeg") > ur.col("outdeg"))
        .collect()
    )
    # only node 0 (in 3 > out 1); node 1 is 1 vs 1; nodes 2,3 are 0 vs 1.
    assert _ids(df) == {0}


def test_arithmetic_inside_predicate(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = (
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
        .filter((ur.col("capacity") + ur.col("indeg")) > 25)
        .collect()
    )
    # capacity+indeg: 0->13, 1->21, 2->30, 3->40
    assert _ids(df) == {2, 3}


def test_boolean_and_or_not(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    base = nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
    both = base.filter((ur.col("region") == "eu") & (ur.col("capacity") > 30)).collect()
    assert _ids(both) == {3}
    either = base.filter((ur.col("region") == "us") | (ur.col("capacity") > 35)).collect()
    assert _ids(either) == {0, 1, 3}
    negated = base.filter(~(ur.col("region") == "us")).collect()
    assert _ids(negated) == {2, 3}


def test_chained_filters_conjoin(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    base = nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
    chained = base.filter(ur.col("capacity") > 15).filter(ur.col("region") == "us").collect()
    combined = base.filter((ur.col("capacity") > 15) & (ur.col("region") == "us")).collect()
    assert _ids(chained) == _ids(combined) == {1}


def test_id_role_in_node_query(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = nodes.with_columns(indeg=ur.degree(EDGES, direction="in")).filter(ur.id() == 0).collect()
    assert _ids(df) == {0}


# --- plain-frame context (evaluated over pyarrow) --------------------------


def test_plain_node_frame_id_role_and_filter(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = nodes.filter(ur.id() >= 2).collect()
    assert _ids(df) == {2, 3}


def test_plain_node_bool_equality():
    table = pa.table({"id": [0, 1, 2], "flag": [True, False, True]})
    nodes = ur.from_arrow(table, id="id")
    df = nodes.filter(ur.col("flag") == True).collect().to_arrow()  # noqa: E712
    assert df.column("id").to_pylist() == [0, 2]


# --- traversal-output context (reserved src/dst columns) -------------------
# NB: filtering a *plain* in-memory/scan edge frame before collect drops the edge
# source today (a row-changing op invalidates the topology + source); that path is
# tracked separately (needs the DataFusion edge pipeline, issue for #18). Role refs
# on traversal outputs — which carry reserved src/dst columns — work now.


def test_hop_output_dst_role():
    df = ur.hop(EDGES, 1).from_([1, 2, 3]).filter(ur.dst() == 0).collect().to_arrow()
    # one hop out from 1,2,3 lands on 0 in every case.
    assert df.num_rows == 3


def test_hop_output_src_role():
    df = ur.hop(EDGES, 1).from_([1, 2, 3]).filter(ur.src() == 1).collect().to_arrow()
    # only the (1 -> 0) edge has src == 1.
    assert df.num_rows == 1


def test_shortest_path_output_dst_role():
    df = ur.shortest_path(EDGES, 1, 0).filter(ur.dst() == 0).collect().to_arrow()
    # the 1 -> 0 path is a single edge landing on 0.
    assert df.num_rows == 1


def test_src_gt_dst_on_hop_output():
    df = ur.hop(EDGES, 1).from_([1, 2, 3]).filter(ur.src() > ur.dst()).collect().to_arrow()
    # (1,0),(2,0),(3,0) all have src > dst.
    assert df.num_rows == 3


# --- honest errors ---------------------------------------------------------


def test_src_role_in_node_query_raises(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    pipeline = nodes.with_columns(pr=ur.pagerank(EDGES)).filter(ur.src() == 0)
    with pytest.raises(NotImplementedError):
        pipeline.collect()


def test_id_role_in_edge_query_raises():
    # ur.id() has no meaning on a traversal's (src, dst) output.
    pipeline = ur.hop(EDGES, 1).from_([1]).filter(ur.id() == 0)
    with pytest.raises(NotImplementedError):
        pipeline.collect()


def test_aggregation_in_predicate_raises():
    # aggregations belong in group_by().agg(), not a row predicate.
    pipeline = ur.hop(EDGES, 1).from_([1]).filter(ur.col("src").sum() > 100)
    with pytest.raises((NotImplementedError, ur.UrsaError)):
        pipeline.collect()
