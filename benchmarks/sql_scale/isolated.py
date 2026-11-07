"""What isolated vertices cost the ``ursa`` binary's algorithms (#326).

A graph with a vertex table has every key as a node, edges or not, and on real
data most keys can have none (37.2M of the Bluesky follows graph's 39.65M).
This runs each algorithm over one R-MAT edge list from ``gen.py`` twice: as
an edge-only graph, and with a vertex table of ``--vertices`` keys (every
node of the edges, then keys with no edges, in a seeded random order). Each
query runs in its own process with no database, so it builds its index; wall
time and peak RSS are the process's, measured as ``run.py`` measures them.

``degree`` is the baseline: building the index and writing one row per node,
with next to no kernel. The rest of an algorithm's time and memory is its
kernel's, and since the kernels run over the nodes with edges only, it
should be about the same with the vertex table as without.

Several binaries can be compared (``--ursa before=PATH --ursa after=PATH``).
Usage::

    python benchmarks/sql_scale/gen.py --out DIR --sizes 5M --string-max 0
    cargo build --release -p ursa-cli
    uv run --group bench python benchmarks/sql_scale/isolated.py \\
        --edges DIR/rmat-5m-int.parquet --vertices 20M --work DIR
"""

from __future__ import annotations

import argparse
import os
import statistics
import sys
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

sys.path.insert(0, str(Path(__file__).resolve().parent))
from gen import parse_size, size_label
from run import REPO, first_number, measure

QUERIES = {
    "degree": "SELECT count(*) AS n, sum(degree) AS s FROM degree(g)",
    "pagerank": "SELECT count(*) AS n, sum(pagerank) AS s FROM pagerank(g)",
    "components": (
        "SELECT count(*) AS n, count(DISTINCT connected_components) AS c "
        "FROM connected_components(g)"
    ),
    "label_propagation": (
        "SELECT count(*) AS n, count(DISTINCT label_propagation) AS c "
        "FROM label_propagation(g, seed => 1)"
    ),
    "louvain": ("SELECT count(*) AS n, count(DISTINCT louvain) AS c FROM louvain(g, seed => 1)"),
    "triangle_count": "SELECT count(*) AS n, sum(triangle_count) AS t FROM triangle_count(g)",
}


def write_vertices(edges: Path, n: int, out: Path, seed: int) -> Path:
    """A vertex table of ``n`` keys: the edges' nodes, then keys with no edge,
    shuffled. Written once per edge file, size and seed."""
    path = out / f"{edges.stem}-v{size_label(n)}.parquet"
    if path.exists():
        return path
    table = pq.read_table(edges, columns=["src", "dst"])
    nodes = np.unique(np.concatenate([table["src"].to_numpy(), table["dst"].to_numpy()]))
    if n < len(nodes):
        raise SystemExit(f"--vertices {n:,} is below the {len(nodes):,} nodes of {edges}")
    # Keys with no edge: integers above every node id.
    extra = np.arange(n - len(nodes), dtype=np.int64) + int(nodes.max()) + 1
    ids = np.concatenate([nodes, extra])
    np.random.default_rng(seed).shuffle(ids)
    pq.write_table(pa.table({"id": ids}), path, compression="zstd")
    print(f"{path.name}: {n:,} keys, {len(nodes):,} with edges", file=sys.stderr)
    return path


def define(edges: Path, vertices: Path | None) -> str:
    sql = f"CREATE VIEW e AS SELECT * FROM '{edges}';\n"
    if vertices is None:
        return sql + (
            "CREATE PROPERTY GRAPH g EDGE TABLES (e SOURCE KEY (src) DESTINATION KEY (dst));\n"
        )
    return sql + (
        f"CREATE VIEW v AS SELECT * FROM '{vertices}';\n"
        "CREATE PROPERTY GRAPH g VERTEX TABLES (v KEY (id)) EDGE TABLES ("
        "e SOURCE KEY (src) REFERENCES v (id) DESTINATION KEY (dst) REFERENCES v (id));\n"
    )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--edges", type=Path, required=True, help="an rmat-*-int.parquet")
    parser.add_argument("--vertices", default="20M", help="vertex table rows (default: 20M)")
    parser.add_argument("--work", type=Path, required=True, help="where the vertex table goes")
    parser.add_argument(
        "--ursa",
        action="append",
        default=[],
        help="LABEL=PATH of a binary; repeat to compare (default: target/release/ursa)",
    )
    parser.add_argument("--steps", default=",".join(QUERIES), help="default: %(default)s")
    parser.add_argument("--repeat", type=int, default=1, help="runs of each (median)")
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--timeout", type=float, default=900)
    parser.add_argument("--mem-limit-gb", type=float, default=11)
    args = parser.parse_args(argv)
    binaries = [b.split("=", 1) if "=" in b else (b, b) for b in args.ursa] or [
        ("ursa", str(REPO / "target" / "release" / "ursa"))
    ]
    binaries = [(label, str(Path(path).resolve())) for label, path in binaries]
    args.work.mkdir(parents=True, exist_ok=True)
    edges = args.edges.resolve()
    vertices = write_vertices(edges, parse_size(args.vertices), args.work.resolve(), args.seed)
    graphs = {"edges only": None, f"+ {args.vertices} vertex table": vertices}
    steps = [s for s in args.steps.split(",") if s]

    results: dict[tuple[str, str, str], tuple[float, float, int | None] | str] = {}
    for label, binary in binaries:
        for graph, vt in graphs.items():
            for step in steps:
                runs = []
                for _ in range(args.repeat):
                    status, wall, rss, out, err = measure(
                        [binary, "-c", define(edges, vt) + QUERIES[step]],
                        cwd=args.work,
                        env=os.environ.copy(),
                        timeout=args.timeout,
                        mem_limit_mb=args.mem_limit_gb * 1024,
                    )
                    if status != "ok":
                        tail = err.strip().splitlines()[-1:] or [""]
                        runs = f"{status.upper()} {tail[0][:80]}"
                        break
                    runs.append((wall, rss, first_number(out)))
                if isinstance(runs, str):
                    results[label, graph, step] = runs
                else:
                    results[label, graph, step] = (
                        statistics.median(r[0] for r in runs),
                        statistics.median(r[1] for r in runs),
                        runs[0][2],
                    )
                print(
                    f"  {label:<8} {graph:<26} {step:<18} {results[label, graph, step]}",
                    file=sys.stderr,
                    flush=True,
                )

    cols = [(label, graph) for label, _ in binaries for graph in graphs]
    print(f"\n{edges.name}, wall time / peak RSS\n")
    print("| step | " + " | ".join(f"{label}: {graph}" for label, graph in cols) + " |")
    print("|---|" + "---|" * len(cols))
    for step in steps:
        cells = []
        for label, graph in cols:
            r = results[label, graph, step]
            cells.append(r if isinstance(r, str) else f"{r[0]:.2f} s / {r[1]:,.0f} MB")
        print(f"| {step} | " + " | ".join(cells) + " |")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
