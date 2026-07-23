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


def test_scan_edges_via_file_url(tmp_path):
    # A file:// URL exercises the full Python -> native object_store path with no
    # credentials (the same code an s3:// path takes, minus the network).
    path = tmp_path / "edges.parquet"
    pq.write_table(pa.table({"s": SRC, "d": DST}), str(path))
    edges = ur.scan_edges("file://" + str(path), src="s", dst="d")
    out = {r["id"]: r["degree"] for r in ur.degree(edges, direction="out").collect().to_dicts()}
    assert out == {0: 2, 1: 1, 2: 1}


def test_scan_nodes_via_file_url(tmp_path):
    npath = tmp_path / "nodes.csv"
    npath.write_text("id,region\n0,us\n1,eu\n")
    nodes = ur.scan_nodes("file://" + str(npath), id="id")
    edges = ur.from_arrow(pa.table({"s": [0], "d": [1]}), src="s", dst="d")
    df = nodes.with_columns(deg=ur.degree(edges, direction="out")).collect().to_polars()
    assert set(df["id"].to_list()) == {0, 1}


def test_storage_options_reach_the_scan_spec():
    # The load-bearing thread-through: storage_options must land in the frame's
    # scan spec and survive a pipeline (this is pure Python, no native call).
    edges = ur.scan_edges(
        "s3://bucket/graph/*.parquet", src="s", dst="d", storage_options={"region": "us-east-1"}
    )
    spec = edges._scan_spec
    assert spec is not None
    assert spec["storage_options"] == {"region": "us-east-1"}
    # survives a property-preserving op (the frame carries the scan spec through)
    enriched = edges.nodes().with_columns(deg=ur.degree(edges))
    assert enriched is not None
    assert edges._scan_spec is not None


def test_store_param_is_honest(tmp_path):
    # store= (obstore) is not supported yet -> a clear error at collect().
    path = tmp_path / "edges.parquet"
    pq.write_table(pa.table({"s": SRC, "d": DST}), str(path))
    edges = ur.scan_edges(str(path), src="s", dst="d", store=object())
    with pytest.raises(NotImplementedError):
        ur.degree(edges).collect()


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


def test_standalone_algorithm_composes_as_a_nodeframe():
    # ur.pagerank(edges) is dual-positioned: a with_columns expression AND a lazy
    # NodeFrame you can filter/sort/head/collect directly.
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")
    df = (
        ur.pagerank(edges)
        .filter(ur.col("pagerank") > 0.05)
        .sort("pagerank", descending=True)
        .head(2)
        .collect()
        .to_polars()
    )
    assert df.columns == ["id", "pagerank"]
    assert len(df) == 2
    assert df["id"].to_list()[0] == 0  # the hub ranks first
    # and the same expression still works inside with_columns
    both = edges.nodes().with_columns(pr=ur.pagerank(edges), deg=ur.degree(edges)).collect()
    assert set(both.columns) == {"id", "pr", "deg"}


def test_attribute_frame_enrichment():
    # A node attribute table (id + region + capacity), enriched with graph metrics
    # and filtered/sorted on both attribute and computed columns.
    nodes = ur.from_arrow(
        pa.table(
            {
                "id": [0, 1, 2, 3],
                "region": ["us", "us", "eu", "eu"],
                "capacity": [10, 20, 30, 40],
            }
        ),
        id="id",
    )
    # hub graph: everyone points at 0
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")

    df = (
        (
            nodes.with_columns(indeg=ur.degree(edges, direction="in"))
            .filter(ur.col("capacity") > 15)  # attribute-column filter
            .sort("indeg", descending=True)
        )
        .collect()
        .to_polars()
    )

    # attributes + computed column both present
    assert set(df.columns) == {"id", "region", "capacity", "indeg"}
    # capacity > 15 keeps ids 1,2,3
    assert set(df["id"].to_list()) == {1, 2, 3}
    # node 1 (in-degree 1 from 0->1) sorts first
    assert df["id"].to_list()[0] == 1
    assert df["region"].to_list()[0] == "us"


def test_neighbors_agg_mean_over_attribute():
    # node 0 is a hub: 1,2,3 all point at it; aggregate a node attribute over
    # each node's in-neighbours.
    nodes = ur.from_arrow(
        pa.table({"id": [0, 1, 2, 3], "capacity": [0.0, 10.0, 20.0, 30.0]}), id="id"
    )
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3], "d": [0, 0, 0]}), src="s", dst="d")
    out = {
        r["id"]: r["nbr_cap"]
        for r in nodes.with_columns(
            nbr_cap=ur.neighbors(edges, direction="in").agg(ur.col("capacity").mean())
        )
        .collect()
        .to_dicts()
    }
    assert abs(out[0] - 20.0) < 1e-9  # mean(10, 20, 30)
    assert out[1] is None  # node 1 has no in-neighbours -> undefined


def test_neighbors_agg_needs_attribute_table():
    # Without a node attribute table (edges.nodes()), there is no attribute to
    # aggregate -> a clear error.
    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    pipeline = edges.nodes().with_columns(x=ur.neighbors(edges).agg(ur.col("capacity").mean()))
    with pytest.raises((NotImplementedError, RuntimeError)):
        pipeline.collect()


def test_attribute_frame_preserves_all_node_rows():
    # A node present in the attribute table but absent from edges keeps its row
    # (LEFT join), with a null/zero-ish computed value.
    nodes = ur.from_arrow(pa.table({"id": [0, 1, 99], "tag": ["a", "b", "c"]}), id="id")
    edges = ur.from_arrow(pa.table({"s": [0], "d": [1]}), src="s", dst="d")
    df = nodes.with_columns(deg=ur.degree(edges, direction="out")).collect().to_polars()
    assert set(df["id"].to_list()) == {0, 1, 99}


def test_scan_nodes_enriches_from_a_file(tmp_path):
    # A node attribute file (id + region + capacity), scanned and enriched with a
    # graph metric; filter on an attribute, sort on the computed column.
    npath = tmp_path / "nodes.csv"
    npath.write_text("id,region,capacity\n0,us,10\n1,us,20\n2,eu,30\n3,eu,40\n")
    nodes = ur.scan_nodes(str(npath), id="id")
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")

    df = (
        nodes.with_columns(indeg=ur.degree(edges, direction="in"))
        .filter(ur.col("capacity") > 15)
        .sort("indeg", descending=True)
        .collect()
        .to_polars()
    )
    assert set(df.columns) == {"id", "region", "capacity", "indeg"}
    assert set(df["id"].to_list()) == {1, 2, 3}  # capacity > 15
    assert df["id"].to_list()[0] == 1  # in-degree 1 (0->1) sorts first
    assert df["region"].to_list()[0] == "us"


def test_scan_nodes_parquet_left_joins_all_rows(tmp_path):
    # A node present in the node file but absent from edges keeps its row (LEFT join).
    npath = tmp_path / "nodes.parquet"
    pq.write_table(pa.table({"id": [0, 1, 99], "tag": ["a", "b", "c"]}), str(npath))
    nodes = ur.scan_nodes(str(npath), id="id")
    edges = ur.from_arrow(pa.table({"s": [0], "d": [1]}), src="s", dst="d")
    df = nodes.with_columns(deg=ur.degree(edges, direction="out")).collect().to_polars()
    assert set(df["id"].to_list()) == {0, 1, 99}


def test_neighbors_agg_n_unique_over_a_string_attribute():
    # node 0 is a hub: 1,2,3 point at it; count distinct regions among its in-neighbours.
    nodes = ur.from_arrow(
        pa.table({"id": [0, 1, 2, 3], "region": ["x", "us", "us", "eu"]}), id="id"
    )
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3], "d": [0, 0, 0]}), src="s", dst="d")
    out = {
        r["id"]: r["nbr_regions"]
        for r in nodes.with_columns(
            nbr_regions=ur.neighbors(edges, direction="in").agg(ur.col("region").n_unique())
        )
        .collect()
        .to_dicts()
    }
    assert out[0] == 2  # {us, eu}
    assert out[1] == 0  # node 1 has no in-neighbours -> 0 distinct (count/n_unique)


def test_neighbors_agg_mean_over_string_is_honest():
    nodes = ur.from_arrow(pa.table({"id": [0, 1], "region": ["us", "eu"]}), id="id")
    edges = ur.from_arrow(pa.table({"s": [1], "d": [0]}), src="s", dst="d")
    pipeline = nodes.with_columns(
        x=ur.neighbors(edges, direction="in").agg(ur.col("region").mean())
    )
    with pytest.raises((NotImplementedError, RuntimeError)):
        pipeline.collect()


def test_hop_reaches_within_n_from_a_seed():
    # path 0 -> 1 -> 2 -> 3, plus branch 1 -> 4
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2, 1], "d": [1, 2, 3, 4]}), src="s", dst="d")
    # two hops from 0: reaches 1 (level 1), then 2 and 4 (level 2)
    rows = ur.hop(edges, n=2).from_([0]).collect().to_dicts()
    reached = {r["dst"] for r in rows}
    assert reached == {1, 2, 4}
    assert all(r["src"] == 0 for r in rows)


def test_hop_direction_in_walks_backwards():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    rows = ur.hop(edges, n=2, direction="in").from_([3]).collect().to_dicts()
    assert {r["dst"] for r in rows} == {1, 2}


def test_hop_composes_with_filter_sort_and_distinct():
    # hub: 1,2,3 -> 0 ; from all three seeds, one hop out all reach 0
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3], "d": [0, 0, 0]}), src="s", dst="d")
    df = (
        ur.hop(edges, n=1)
        .from_([1, 2, 3])
        .filter(ur.col("dst") == 0)
        .sort("src", descending=True)
        .collect()
        .to_polars()
    )
    assert df["dst"].to_list() == [0, 0, 0]
    assert df["src"].to_list() == [3, 2, 1]  # sorted descending by seed


def test_hop_seeds_from_a_nodeframe():
    # seeds provided as a NodeFrame (its id column drives the seed set)
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    seeds = ur.from_arrow(pa.table({"id": [0]}), id="id")
    rows = ur.hop(edges, n=1).from_(seeds).collect().to_dicts()
    assert {r["dst"] for r in rows} == {1}


def test_hop_requires_seeds():
    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    with pytest.raises(NotImplementedError):
        ur.hop(edges, n=1).collect()


def test_plain_edge_frame_collect_returns_the_edges():
    # A bare EdgeFrame (no traversal/algorithm) now materializes its own edge rows
    # rather than raising (the plain-source collect path).
    edges = ur.from_arrow(pa.table({"s": SRC, "d": DST}), src="s", dst="d")
    got = edges.collect()
    assert set(got.columns) == {"s", "d"}
    assert sorted((r["s"], r["d"]) for r in got.to_dicts()) == sorted(zip(SRC, DST, strict=True))


def test_shortest_path_returns_ordered_edges():
    # path 0 -> 1 -> 2 -> 3
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    rows = ur.shortest_path(edges, 0, 3).collect().to_dicts()
    assert [(r["src"], r["dst"], r["hop"]) for r in rows] == [(0, 1, 0), (1, 2, 1), (2, 3, 2)]


def test_shortest_path_unreachable_is_empty():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    # no directed path 3 -> 0
    assert ur.shortest_path(edges, 3, 0).collect().to_dicts() == []


def test_shortest_path_direction_in():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    rows = ur.shortest_path(edges, 3, 0, direction="in").collect().to_dicts()
    assert [(r["src"], r["dst"]) for r in rows] == [(3, 2), (2, 1), (1, 0)]


def test_shortest_path_over_scan_source(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("s,d\n0,1\n1,2\n2,3\n")
    edges = ur.scan_edges(str(path), src="s", dst="d")
    rows = ur.shortest_path(edges, 0, 3).collect().to_dicts()
    assert len(rows) == 3


def test_shortest_path_weighted_takes_the_cheaper_route():
    # 0->1->2 (cost 1+1) vs direct 0->2 (cost 5): weighted Dijkstra picks the
    # cheaper two-hop path, unlike unweighted BFS (see tests/test_weighted.py).
    edges = ur.from_arrow(
        pa.table({"s": [0, 1, 0], "d": [1, 2, 2], "cost": [1.0, 1.0, 5.0]}), src="s", dst="d"
    )
    path = ur.shortest_path(edges, 0, 2, weight=ur.col("cost")).collect().to_dicts()
    assert [r["src"] for r in path] == [0, 1]


def test_describe_summarizes_the_graph():
    # directed triangle: 3 nodes, 3 edges, density 0.5, avg_degree 1
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    row = ur.describe(edges).collect().to_dicts()[0]
    assert row["n_nodes"] == 3
    assert row["n_edges"] == 3
    assert abs(row["density"] - 0.5) < 1e-12
    assert abs(row["avg_degree"] - 1.0) < 1e-12
    assert row["n_components"] is None  # gated behind full=


def test_describe_full_computes_components():
    # two disjoint edges -> two weakly-connected components
    edges = ur.from_arrow(pa.table({"s": [0, 2], "d": [1, 3]}), src="s", dst="d")
    row = ur.describe(edges, full=True).collect().to_dicts()[0]
    assert row["n_components"] == 2


def test_describe_over_scan_source(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("s,d\n0,1\n1,2\n2,0\n")
    edges = ur.scan_edges(str(path), src="s", dst="d")
    row = ur.describe(edges).collect().to_dicts()[0]
    assert row["n_nodes"] == 3
    assert row["n_edges"] == 3


def test_avg_path_length_and_diameter():
    # directed triangle 0->1->2->0: avg path length 1.5, diameter 2
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    assert abs(ur.avg_path_length(edges) - 1.5) < 1e-12
    assert ur.diameter(edges, approximate=False) == 2
    assert isinstance(ur.diameter(edges), int)


def test_path_stats_on_a_line():
    # line 0->1->2->3: diameter 3 (0 to 3)
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    assert ur.diameter(edges, approximate=False) == 3


def test_path_stats_over_scan_source(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("s,d\n0,1\n1,2\n2,0\n")
    edges = ur.scan_edges(str(path), src="s", dst="d")
    assert abs(ur.avg_path_length(edges) - 1.5) < 1e-12


def test_topology_built_once_per_frame():
    # The index-preservation contract: one frame -> the CSR is built exactly once,
    # shared by every graph op over it, and reused across collect()s.
    from ursa._execute import _native

    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")
    before = _native()._topology_build_count()

    # three algorithms over ONE frame in a single pipeline
    df = (
        edges.nodes()
        .with_columns(
            pr=ur.pagerank(edges),
            indeg=ur.degree(edges, direction="in"),
            tri=ur.triangle_count(edges),
        )
        .collect()
        .to_polars()
    )
    assert set(df.columns) == {"id", "pr", "indeg", "tri"}
    after_first = _native()._topology_build_count()
    assert after_first - before == 1  # built once for all three algorithms

    # a second collect on the same frame reuses the cached index -> no rebuild
    edges.nodes().with_columns(pr=ur.pagerank(edges)).collect()
    assert _native()._topology_build_count() == after_first

    # an eager stat over the same frame also reuses it
    ur.density(edges)
    assert _native()._topology_build_count() == after_first


def test_index_rides_the_preserve_drop_rail():
    # The cached handle is preserved by property-only ops and dropped by
    # structural ones (matching the source/scan invalidation).
    edges = ur.from_arrow(pa.table({"s": [0, 1], "d": [1, 2]}), src="s", dst="d")
    ur.degree(edges).collect()  # force a build
    built = edges._index
    assert built is not None
    # property-only op (rename) carries the same handle forward
    assert edges.rename({})._index is built
    # structural op (distinct) drops it
    assert edges.distinct()._index is None


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
