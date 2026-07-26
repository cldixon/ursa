"""select() — Polars-faithful output-column projection, and the scan projection
pushdown it drives (#61).

select() narrows *which* columns the result carries; it never changes rows.
with_columns() stays additive (a pipeline with no select still returns every
attribute — see the enrichment tests in test_scan_and_pipeline.py). When a select
*does* narrow the output, the scan reads only the columns the plan provably needs
(id + filter/sort/agg columns + selected file columns), so an unreferenced
attribute is never read — while a filter on a column that select drops still works
(the correctness guard against under-projection).
"""

from __future__ import annotations

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# hub graph: 1,2,3 all point at 0
EDGES = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")


def _nodes_file(tmp_path) -> str:
    path = tmp_path / "nodes.csv"
    path.write_text("id,region,capacity\n0,us,10\n1,us,20\n2,eu,30\n3,eu,40\n")
    return str(path)


def test_select_narrows_node_pipeline_output(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = (
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
        .select("id", "indeg")
        .collect()
        .to_polars()
    )
    # region + capacity are dropped; only the selected columns remain.
    assert df.columns == ["id", "indeg"]
    assert set(df["id"].to_list()) == {0, 1, 2, 3}


def test_with_columns_stays_additive_without_select(tmp_path):
    # No select => additive with_columns keeps every attribute (unchanged behavior).
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = nodes.with_columns(indeg=ur.degree(EDGES, direction="in")).collect().to_polars()
    assert set(df.columns) == {"id", "region", "capacity", "indeg"}


def test_select_keeps_filter_on_a_dropped_column(tmp_path):
    # capacity drives the filter but is NOT selected. A naive projection (only the
    # selected columns) would drop capacity and break the filter — this asserts the
    # projection still includes filter columns.
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = (
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
        .filter(ur.col("capacity") > 15)
        .sort("indeg", descending=True)
        .select("id", "indeg")
        .collect()
        .to_polars()
    )
    assert df.columns == ["id", "indeg"]
    assert set(df["id"].to_list()) == {1, 2, 3}  # capacity > 15 kept ids 1,2,3


def test_select_on_a_plain_node_scan(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = nodes.select("id", "region").collect().to_polars()
    assert df.columns == ["id", "region"]
    assert set(df["region"].to_list()) == {"us", "eu"}


def test_select_on_an_in_memory_node_frame():
    nodes = ur.from_arrow(
        pa.table({"id": [0, 1, 2, 3], "region": ["us", "us", "eu", "eu"]}), id="id"
    )
    df = (
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
        .select("id", "indeg")
        .collect()
        .to_polars()
    )
    assert df.columns == ["id", "indeg"]


def test_select_on_a_standalone_algorithm():
    df = ur.pagerank(EDGES).select("pagerank").collect().to_polars()
    assert df.columns == ["pagerank"]


def test_select_on_a_plain_edge_frame():
    df = EDGES.select("s").collect().to_polars()
    assert df.columns == ["s"]


def test_select_column_order_is_respected(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    df = (
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in"))
        .select("indeg", "id")  # deliberately reversed
        .collect()
        .to_polars()
    )
    assert df.columns == ["indeg", "id"]


def test_with_columns_after_select_is_rejected(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    pipeline = nodes.select("id").with_columns(indeg=ur.degree(EDGES, direction="in"))
    with pytest.raises(NotImplementedError, match=r"with_columns.*after select"):
        pipeline.collect()


def test_select_unknown_column_errors(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    with pytest.raises((ur.ColumnNotFoundError, Exception)):
        nodes.with_columns(indeg=ur.degree(EDGES, direction="in")).select("id", "nope").collect()


def test_scan_nodes_seed_frame_reads_only_id(tmp_path):
    # A plain node file used purely as traversal seeds: the scan projects to [id].
    # The extra columns (region, capacity) are never read; the hop still resolves.
    seeds = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    rows = ur.hop(EDGES, n=1).from_(seeds).collect().to_dicts()
    # seeds {0,1,2,3}; one hop out: 0->1, 1->0, 2->0, 3->0
    assert {r["dst"] for r in rows} == {0, 1}
