"""Pure-Python tests: the expression dialect and lazy plan building.

These need no native extension — they exercise the parts of the skeleton that are
fully live today.
"""

import ursa as ur


def test_expression_tree_and_repr():
    e = ur.col("amount") * ur.col("fx_rate") > ur.lit(1000)
    assert e.kind == "binary"
    assert repr(e) == "((col('amount') * col('fx_rate')) > lit(1000))"


def test_role_references():
    assert ur.src().kind == "src"
    assert ur.dst().kind == "dst"
    assert ur.id().kind == "id"


def test_aggregation_expr():
    e = ur.col("capacity_gbps").mean()
    assert repr(e) == "col('capacity_gbps').mean()"


def test_scan_edges_records_roles_lazily():
    edges = ur.scan_edges("web-google.csv", src="FromNodeId", dst="ToNodeId")
    assert isinstance(edges, ur.EdgeFrame)
    assert edges.src_col == "FromNodeId"
    assert edges.dst_col == "ToNodeId"
    # lazy: nothing collected, plan has exactly the scan step
    assert "scan_edges" in edges.explain()


def test_index_preservation_contract():
    edges = ur.scan_edges("g.parquet", src="a", dst="b")
    # with_columns preserves the index...
    kept = edges.with_columns(x=ur.col("a"))
    assert kept._has_index is True
    # ...filter (row-changing on an EdgeFrame) drops it.
    dropped = edges.filter(ur.col("w") > 0)
    assert dropped._has_index is False
    assert "dropped" in dropped.explain()


def test_reverse_is_metadata_only_role_swap():
    edges = ur.scan_edges("g.parquet", src="a", dst="b")
    rev = edges.reverse()
    assert (rev.src_col, rev.dst_col) == ("b", "a")


def test_nodes_derives_a_nodeframe():
    edges = ur.scan_edges("g.parquet", src="a", dst="b")
    nodes = edges.nodes()
    assert isinstance(nodes, ur.NodeFrame)


def test_composed_pipeline_builds_lazy_plan():
    # Pure-Python structural check: a composed pipeline composes into a lazy
    # NodeFrame plan (execution is covered in tests/test_scan_and_pipeline.py).
    edges = ur.scan_edges("g.parquet", src="a", dst="b")
    nodes = (
        edges.nodes()
        .with_columns(
            pr=ur.pagerank(edges, damping=0.85),
            deg=ur.degree(edges, direction="in"),
        )
        .sort("pr", descending=True)
        .head(20)
    )
    assert isinstance(nodes, ur.NodeFrame)
    ops = [step.op for step in nodes._plan]
    assert ops == ["scan_edges", "nodes", "with_columns", "sort", "head"]
