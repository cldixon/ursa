"""Ursa quickstart — a tour of what the v0.1 library can do today.

Run it from the repo root after building the extension (``uv sync`` or
``maturin develop``):

    uv run python examples/quickstart.py

Everything is Arrow under the hood, so results flow out to polars/pyarrow
zero-copy. `polars` is used here only for pretty-printing.
"""

from __future__ import annotations

import pyarrow as pa

import ursa as ur


def main() -> None:
    print(f"ursa {ur.__version__}  (native core {ur.__core_version__})\n")

    # A small directed graph: two triangles (0-1-2 and 3-4-5) joined by a bridge
    # 2 -> 3, plus two extra nodes 6, 7 that both point at hub node 0.
    src = [0, 1, 2, 3, 4, 5, 2, 6, 7]
    dst = [1, 2, 0, 4, 5, 3, 3, 0, 0]
    edges = ur.from_arrow(pa.table({"s": src, "d": dst}), src="s", dst="d")

    # -- 1. Standalone node-valued algorithms -------------------------------
    # Each returns an (id, value) frame; .collect() runs it as one DataFusion
    # plan and .to_polars() hands the Arrow result back zero-copy.
    print("== standalone algorithms ==")
    print(ur.pagerank(edges, damping=0.85).collect().to_polars())
    print("in-degree:      ", ur.degree(edges, direction="in").collect().to_dicts())
    print("components:     ", ur.connected_components(edges).collect().to_dicts())
    print("triangles:      ", ur.triangle_count(edges).collect().to_dicts())
    print("clustering:     ", ur.clustering_coefficient(edges).collect().to_dicts())

    # Centrality and community detection over the same topology.
    print("closeness:      ", ur.closeness(edges).collect().to_dicts())
    print("betweenness:    ", ur.betweenness(edges).collect().to_dicts())
    print("communities:    ", ur.louvain(edges, seed=1).collect().to_dicts())
    print("labels (LPA):   ", ur.label_propagation(edges, seed=1).collect().to_dicts())

    # -- 2. A composed pipeline (one lazy plan) -----------------------------
    # Several metrics computed together, then filtered / sorted / truncated.
    top = (
        edges.nodes()
        .with_columns(
            pr=ur.pagerank(edges),
            indeg=ur.degree(edges, direction="in"),
            tri=ur.triangle_count(edges),
        )
        .filter(ur.col("indeg") > 0)
        .sort("pr", descending=True)
        .head(5)
    )
    print("\n== composed pipeline ==")
    print(top.explain())  # inspect the lazy plan without running it
    print(top.collect().to_polars())

    # -- 3. Attribute enrichment --------------------------------------------
    # A node attribute table joined with graph metrics; filter on an attribute
    # column, sort on a computed one. The LEFT join keeps every attribute row.
    nodes = ur.from_arrow(
        pa.table(
            {
                "id": [0, 1, 2, 3, 4, 5, 6, 7],
                "team": ["red", "red", "red", "blue", "blue", "blue", "red", "blue"],
                "seniority": [5, 2, 4, 1, 3, 2, 1, 5],
            }
        ),
        id="id",
    )
    enriched = (
        nodes.with_columns(
            pr=ur.pagerank(edges),
            indeg=ur.degree(edges, direction="in"),
            # neighbour aggregation: average seniority of each node's predecessors
            nbr_seniority=ur.neighbors(edges, direction="in").agg(ur.col("seniority").mean()),
        )
        .filter(ur.col("seniority") > 1)
        .sort("pr", descending=True)
    )
    print("\n== attribute enrichment + neighbour aggregation ==")
    print(enriched.collect().to_polars())

    # -- 4. Traversal: k-hop reachability -----------------------------------
    # ur.hop returns an EdgeFrame (src = seed, dst = reached). Here: everything
    # reachable within 2 hops from node 0, then compose a relational tail on it.
    print("\n== 2-hop reachability from node 0 ==")
    reach = ur.hop(edges, n=2).from_([0]).sort("dst").collect().to_polars()
    print(reach)

    # ur.shortest_path returns the path as an EdgeFrame (src, dst, hop-in-order).
    print("\n== shortest path 0 -> 5 ==")
    print(ur.shortest_path(edges, 0, 5).collect().to_polars())

    # -- 5. Whole-graph summary + path stats --------------------------------
    print("\n== describe ==")
    print(ur.describe(edges, full=True).collect().to_polars())
    print("density:         ", ur.density(edges))
    print("avg_path_length: ", ur.avg_path_length(edges))
    print("diameter (exact):", ur.diameter(edges, approximate=False))

    # -- 6. Read from files, write results back -----------------------------
    # scan_edges / scan_nodes read Parquet/CSV through a DataFusion scan; the node
    # file is joined to the computed metrics by id. sink_* writes the result.
    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as tmp:
        csv = Path(tmp) / "links.csv"
        csv.write_text("from,to,latency_ms\n0,1,5\n1,2,7\n2,0,3\n2,3,9\n3,4,2\n4,3,4\n")
        ncsv = Path(tmp) / "towers.csv"
        ncsv.write_text("id,site\n0,a\n1,a\n2,b\n3,b\n4,c\n")

        scanned = ur.scan_edges(str(csv), src="from", dst="to")
        towers = ur.scan_nodes(str(ncsv), id="id")
        result = towers.with_columns(pr=ur.pagerank(scanned)).sort("pr", descending=True)

        out = Path(tmp) / "metrics.parquet"
        result.collect().sink_parquet(str(out))
        print("\n== scan CSV edges + nodes -> compute -> sink Parquet ==")
        import pyarrow.parquet as pq

        print(pq.read_table(str(out)).to_pydict())

    # The same scans read from object storage — s3://, gs://, az:// (and file://).
    # Credentials/config come from storage_options, layered over the backend's
    # default credential chain (env vars / instance profile). Not run here (needs
    # a bucket), but the call is identical to the local scans above:
    #
    #     edges = ur.scan_edges(
    #         "s3://telco-lake/graph/links/*.parquet",
    #         src="tower_a", dst="tower_b",
    #         storage_options={"region": "us-east-1"},
    #     )
    #     nodes = ur.scan_nodes("s3://telco-lake/graph/towers/*.parquet", id="tower_id")
    #     ur.pagerank(edges).collect().to_polars()   # projection pushed into Parquet

    # -- 7. The expression dialect is Polars-shaped (pure Python) -----------
    predicate = (ur.col("pr") * ur.lit(100) > ur.lit(5)) & (ur.col("indeg") >= ur.lit(2))
    print("\n== expression dialect ==\n", repr(predicate))


if __name__ == "__main__":
    main()
