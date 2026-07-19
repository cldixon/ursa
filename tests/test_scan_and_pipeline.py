"""End-to-end tests for the two new collect() paths: scan_edges file sources and
composed with_columns pipelines. Skipped if the native extension isn't built.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `uv sync`)"
)

# diamond: 0->1, 0->2, 1->2, 2->0
SRC = [0, 0, 1, 2]
DST = [1, 2, 2, 0]


def test_scan_csv_source_collects(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("from,to,weight\n0,1,0.5\n0,2,0.1\n1,2,0.2\n2,0,0.9\n")
    edges = ur.scan_edges(str(path), src="from", dst="to")
    out = {r["id"]: r["degree"] for r in ur.degree(edges, direction="out").collect().to_dicts()}
    assert out == {0: 2, 1: 1, 2: 1}


def test_scan_parquet_source_collects(tmp_path):
    path = tmp_path / "edges.parquet"
    pq.write_table(pa.table({"s": SRC, "d": DST, "extra": [9, 9, 9, 9]}), str(path))
    edges = ur.scan_edges(str(path), src="s", dst="d")
    rows = ur.pagerank(edges).collect().to_dicts()
    assert abs(sum(r["pagerank"] for r in rows) - 1.0) < 1e-6


def test_triangle_count_is_real_now():
    # single triangle 0-1-2 -> each node in exactly one triangle
    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    counts = {r["id"]: r["triangle_count"] for r in ur.triangle_count(edges).collect().to_dicts()}
    assert counts == {0: 1, 1: 1, 2: 1}


def test_clustering_coefficient_collects():
    # triangle 0-1-2: every node fully clustered -> 1.0
    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    cc = {
        r["id"]: r["clustering_coefficient"]
        for r in ur.clustering_coefficient(edges).collect().to_dicts()
    }
    assert cc == {0: 1.0, 1: 1.0, 2: 1.0}


def test_density_eager_scalar():
    # directed triangle: 3 edges of 6 possible -> 0.5
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    d = ur.density(edges)
    assert isinstance(d, float)
    assert abs(d - 0.5) < 1e-12


def test_density_over_scan_source(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("s,d\n0,1\n1,2\n2,0\n")
    edges = ur.scan_edges(str(path), src="s", dst="d")
    assert abs(ur.density(edges) - 0.5) < 1e-12


def test_composed_pipeline_collects():
    # node 0 is a hub everyone points at
    src = [1, 2, 3, 0]
    dst = [0, 0, 0, 1]
    edges = ur.from_arrow(pa.table({"s": src, "d": dst}), src="s", dst="d")
    result = (
        edges.nodes()
        .with_columns(
            pr=ur.pagerank(edges, damping=0.85),
            indeg=ur.degree(edges, direction="in"),
        )
        .filter(ur.col("indeg") > 0)
        .sort("pr", descending=True)
        .head(2)
    ).collect()

    df = result.to_polars()
    assert set(df.columns) == {"id", "pr", "indeg"}
    assert len(df) <= 2
    # every surviving row has in-degree > 0
    assert all(v > 0 for v in df["indeg"].to_list())
    # sorted by pagerank descending
    prs = df["pr"].to_list()
    assert prs == sorted(prs, reverse=True)
    # the hub (node 0, in-degree 3) has the highest pagerank -> first row
    assert df["id"].to_list()[0] == 0


def test_composed_pipeline_over_scan_source(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("s,d\n1,0\n2,0\n3,0\n0,1\n")
    edges = ur.scan_edges(str(path), src="s", dst="d")
    df = (
        (
            edges.nodes()
            .with_columns(deg=ur.degree(edges, direction="in"))
            .sort("deg", descending=True)
            .head(1)
        )
        .collect()
        .to_polars()
    )
    assert df["id"].to_list() == [0]  # node 0 has the highest in-degree


def test_sink_parquet_and_csv_roundtrip(tmp_path):
    import pyarrow.parquet as pq

    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    result = ur.degree(edges, direction="out").collect()

    pq_path = tmp_path / "out.parquet"
    result.sink_parquet(str(pq_path))
    assert pq.read_table(str(pq_path)).num_rows == 3

    csv_path = tmp_path / "out.csv"
    result.sink_csv(str(csv_path))
    assert "degree" in csv_path.read_text().splitlines()[0]


def test_nodeframe_sink_collects_then_writes(tmp_path):
    import pyarrow.parquet as pq

    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    out = tmp_path / "pipeline.parquet"
    edges.nodes().with_columns(deg=ur.degree(edges)).sink_parquet(str(out))
    assert set(pq.read_table(str(out)).column_names) == {"id", "deg"}


def test_unsupported_filter_predicate_is_honest():
    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    pipeline = (
        edges.nodes()
        .with_columns(pr=ur.pagerank(edges))
        .filter(
            ur.col("pr") > ur.col("pr")  # col op col: not a simple col-op-literal
        )
    )
    with pytest.raises(NotImplementedError):
        pipeline.collect()
