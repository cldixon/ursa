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
        nodes.with_columns(pr=ur.pagerank(edges), indeg=ur.degree(edges, direction="in"))
        .filter(ur.col("seniority") > 1)
        .sort("pr", descending=True)
    )
    print("\n== attribute enrichment ==")
    print(enriched.collect().to_polars())

    # -- 4. Whole-graph stat (eager) ----------------------------------------
    print("\n== density ==", ur.density(edges))

    # -- 5. Read from a file, write results back ----------------------------
    # scan_edges reads Parquet/CSV through a DataFusion scan (projection pushed
    # down); sink_* writes the materialized result.
    import tempfile
    from pathlib import Path

    with tempfile.TemporaryDirectory() as tmp:
        csv = Path(tmp) / "links.csv"
        csv.write_text("from,to,latency_ms\n0,1,5\n1,2,7\n2,0,3\n2,3,9\n3,4,2\n4,3,4\n")

        scanned = ur.scan_edges(str(csv), src="from", dst="to")
        result = scanned.nodes().with_columns(pr=ur.pagerank(scanned)).sort("pr", descending=True)

        out = Path(tmp) / "metrics.parquet"
        result.collect().sink_parquet(str(out))
        print("\n== scan CSV -> compute -> sink Parquet ==")
        import pyarrow.parquet as pq

        print(pq.read_table(str(out)).to_pydict())

    # -- 6. The expression dialect is Polars-shaped (pure Python) -----------
    predicate = (ur.col("pr") * ur.lit(100) > ur.lit(5)) & (ur.col("indeg") >= ur.lit(2))
    print("\n== expression dialect ==\n", repr(predicate))


if __name__ == "__main__":
    main()
