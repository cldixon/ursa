"""GRAPH_TABLE against the traversals, on a synthetic R-MAT graph (#325).

A bound pattern expands its edges through the graph index, as ``hop`` does,
and a path search with a known last node stops at it, as ``shortest_path``
does. This compares each pattern with the traversal it should be close to,
and with the same pattern planned as joins (its bound condition written
``… OR false``, which binds nothing, so the pattern is planned as in 0.7).

Given an ``rmat-<size>-int.parquet`` from ``gen.py``, it writes a vertex
table of every node slot (``id``, ``name``) as Parquet and as gzipped CSV,
defines three graphs in a fresh ``.ursa`` database (edges only; with the
Parquet vertex table; with the gzipped one), stores each graph's index with
one ``hop``, then runs every step in a new ``ursa`` process, so each reads
the stored index, as a user's next query would. Each step's wall time is the
median of ``--repeat`` runs; its result rows are printed so the pairs can be
checked against each other.

Usage::

    cargo build --release -p ursa-cli
    uv run --no-project --with numpy --with pyarrow \\
        python benchmarks/sql_scale/patterns.py --edges DIR/rmat-10m-int.parquet --work DIR
"""

from __future__ import annotations

import argparse
import statistics
import subprocess
import sys
import time
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.csv as pv
import pyarrow.parquet as pq

REPO = Path(__file__).resolve().parents[2]


def vertex_tables(edges: Path, work: Path) -> tuple[Path, Path]:
    """Every node slot of the R-MAT graph as a vertex table, Parquet and CSV.gz."""
    table = pq.read_table(edges, columns=["src", "dst"])
    top = max(pc.max(table["src"]).as_py(), pc.max(table["dst"]).as_py())
    slots = 1 << int(top).bit_length()
    ids = pa.array(np.arange(slots, dtype=np.int64))
    names = pc.binary_join_element_wise("v", pc.cast(ids, pa.string()), "")
    nodes = pa.table({"id": ids, "name": names})
    parquet = work / "nodes.parquet"
    pq.write_table(nodes, parquet, compression="zstd", row_group_size=1 << 20)
    gz = work / "nodes.csv.gz"
    with pa.CompressedOutputStream(str(gz), "gzip") as out:
        pv.write_csv(nodes, out)
    return parquet, gz


def pick(edges: Path) -> tuple[int, int]:
    """A seed with a few dozen out-edges, and a node 3 or more steps from it."""
    table = pq.read_table(edges, columns=["src", "dst"])
    src = table["src"].to_numpy()
    dst = table["dst"].to_numpy()
    ids, counts = np.unique(src, return_counts=True)
    seed = int(ids[np.argmin(np.abs(counts - 40))])
    # Breadth-first along the edges, to the first node 3 steps out.
    order = np.argsort(src, kind="stable")
    s, d = src[order], dst[order]
    frontier, seen = {seed}, {seed}
    for _ in range(3):
        nxt = set()
        for u in frontier:
            lo, hi = np.searchsorted(s, u), np.searchsorted(s, u, side="right")
            for w in d[lo:hi].tolist():
                if w not in seen:
                    seen.add(w)
                    nxt.add(w)
        frontier = nxt
    return seed, min(frontier)


def steps(seed: int, target: int) -> list[tuple[str, str]]:
    s, t = seed, target
    return [
        ("hop n=1", f"SELECT count(*) FROM hop(g, seeds => [{s}], n => 1)"),
        (
            "match 1 edge",
            f"SELECT count(*) FROM GRAPH_TABLE (g MATCH (a WHERE a.id = {s})-[]->(b) "
            "COLUMNS (b.id))",
        ),
        (
            "match 1 edge, joins",
            f"SELECT count(*) FROM GRAPH_TABLE (g MATCH (a WHERE a.id = {s} OR false)-[]->(b) "
            "COLUMNS (b.id))",
        ),
        ("hop n=2 (vertex table)", f"SELECT count(*) FROM hop(gv, seeds => [{s}], n => 2)"),
        (
            "match 2 edges (vertex table)",
            f"SELECT count(*) FROM GRAPH_TABLE (gv MATCH (a WHERE a.id = {s})-[]->(b)-[]->(c) "
            "COLUMNS (c.id))",
        ),
        (
            "match 2 edges (vertex table), joins",
            f"SELECT count(*) FROM GRAPH_TABLE (gv MATCH (a WHERE a.id = {s} OR false)-[]->(b)"
            "-[]->(c) COLUMNS (c.id))",
        ),
        (
            "match 2 edges, names (Parquet)",
            f"SELECT count(DISTINCT c) FROM GRAPH_TABLE (gv MATCH (a WHERE a.id = {s})-[]->(b)"
            "-[]->(c) COLUMNS (c.name AS c))",
        ),
        (
            "match 2 edges, names (CSV.gz)",
            f"SELECT count(DISTINCT c) FROM GRAPH_TABLE (gz MATCH (a WHERE a.id = {s})-[]->(b)"
            "-[]->(c) COLUMNS (c.name AS c))",
        ),
        (
            "shortest_path",
            f"SELECT count(*) FROM shortest_path(gv, source => {s}, target => {t})",
        ),
        (
            "ANY SHORTEST to a target",
            "SELECT n FROM GRAPH_TABLE (gv MATCH p = ANY SHORTEST "
            f"(a WHERE a.id = {s})-[]->*(b WHERE b.id = {t}) COLUMNS (path_length(p) AS n))",
        ),
        (
            "ANY SHORTEST to a target (CSV.gz)",
            "SELECT n FROM GRAPH_TABLE (gz MATCH p = ANY SHORTEST "
            f"(a WHERE a.id = {s})-[]->*(b WHERE b.id = {t}) COLUMNS (path_length(p) AS n))",
        ),
        (
            "ANY SHORTEST, as 0.7 (CSV.gz)",
            "SELECT n FROM GRAPH_TABLE (gz MATCH p = ANY SHORTEST "
            f"(a WHERE a.id = {s} OR false)-[]->*(b WHERE b.id = {t} OR false) "
            "COLUMNS (a.name, b.name, path_length(p) AS n))",
        ),
    ]


def run(ursa: Path, db: Path, sql: str) -> tuple[float, str]:
    start = time.perf_counter()
    done = subprocess.run(
        [str(ursa), str(db), "--format", "csv", "-c", sql],
        capture_output=True,
        text=True,
        check=False,
    )
    wall = time.perf_counter() - start
    if done.returncode != 0:
        raise SystemExit(f"{sql}\n{done.stderr}")
    return wall, done.stdout.strip().splitlines()[-1]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--edges", type=Path, required=True, help="an rmat-<size>-int.parquet")
    parser.add_argument("--work", type=Path, required=True, help="a scratch directory")
    parser.add_argument("--ursa", type=Path, default=REPO / "target/release/ursa")
    parser.add_argument("--repeat", type=int, default=3)
    args = parser.parse_args(argv)
    args.work.mkdir(parents=True, exist_ok=True)
    parquet, gz = vertex_tables(args.edges, args.work)
    seed, target = pick(args.edges)
    db = args.work / "patterns.ursa"
    for old in args.work.glob("patterns.ursa*"):
        subprocess.run(["rm", "-rf", str(old)], check=True)
    edges = args.edges.resolve()
    define = f"""
        CREATE VIEW e AS SELECT * FROM '{edges}';
        CREATE VIEW v AS SELECT * FROM '{parquet.resolve()}';
        CREATE VIEW vgz AS SELECT * FROM '{gz.resolve()}';
        CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));
        CREATE PROPERTY GRAPH gv VERTEX TABLES (v KEY (id))
          EDGE TABLES (e SOURCE KEY (src) REFERENCES v (id)
                         DESTINATION KEY (dst) REFERENCES v (id));
        CREATE PROPERTY GRAPH gz VERTEX TABLES (vgz KEY (id))
          EDGE TABLES (e SOURCE KEY (src) REFERENCES vgz (id)
                         DESTINATION KEY (dst) REFERENCES vgz (id));
        SELECT count(*) FROM hop(g, seeds => [{seed}], n => 1);
        SELECT count(*) FROM hop(gv, seeds => [{seed}], n => 1);
        SELECT count(*) FROM hop(gz, seeds => [{seed}], n => 1);
    """
    run(args.ursa, db, define)
    print(f"{edges.name}: seed {seed}, target {target}", file=sys.stderr)
    print("| step | median s | result |\n|---|---|---|")
    for name, sql in steps(seed, target):
        times, result = [], ""
        for _ in range(args.repeat):
            wall, result = run(args.ursa, db, sql)
            times.append(wall)
        print(f"| {name} | {statistics.median(times):.3f} | {result} |", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
