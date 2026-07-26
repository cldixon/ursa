"""The ``ursa_bench`` command line — a ``typer`` app with ``rich`` output.

Subcommands map onto the flywheel's stages:

* ``libraries`` / ``algorithms`` / ``datasets`` — inspect what can be raced;
* ``generate`` — materialise a dataset's Parquet (synthetic build or real download);
* ``run`` — measure a filtered slice of the matrix, writing raw rows to Parquet;
* ``report`` — render a results Parquet into a leaderboard + the gaps to-do list;
* ``smoke`` — the tiny end-to-end run CI uses to prove the harness still works.

The measurement (``run``) and the analysis (``report``) are deliberately separate
commands over a Parquet file, so results outlive any single rendering.
"""

from __future__ import annotations

from datetime import datetime, timezone
from pathlib import Path

import typer
from rich.console import Console
from rich.table import Table

from . import algorithms as algos_mod
from . import datasets as ds_mod
from . import report as report_mod
from .adapters import get_adapter, known_libraries
from .results import STATUS_OK
from .runner import RunConfig, run_matrix

app = typer.Typer(
    add_completion=False,
    help="Ursa's cross-library graph-benchmark flywheel.",
    no_args_is_help=True,
)
console = Console()

DEFAULT_RESULTS = Path(__file__).resolve().parent.parent / "results" / "latest.parquet"


def _run_id() -> str:
    return datetime.now(timezone.utc).strftime("run-%Y%m%dT%H%M%SZ")


@app.command()
def libraries() -> None:
    """List the libraries the harness can race, with installed versions."""
    table = Table("library", "version", "status", header_style="bold")
    for name in known_libraries():
        try:
            table.add_row(name, get_adapter(name).version(), "[green]ok[/green]")
        except Exception as exc:
            table.add_row(name, "—", f"[red]{type(exc).__name__}[/red]")
    console.print(table)


@app.command()
def algorithms() -> None:
    """List the canonical algorithms and their reference definitions."""
    table = Table("algorithm", "view", "compare", "description", header_style="bold")
    for name in algos_mod.names():
        a = algos_mod.get(name)
        table.add_row(name, a.view, a.compare, a.description)
    console.print(table)


@app.command()
def datasets() -> None:
    """List the dataset catalog (synthetic generators + real graphs)."""
    table = Table("dataset", "kind", "directed", "note", header_style="bold")
    for name in ds_mod.names():
        spec = ds_mod.resolve(name)
        table.add_row(name, spec.kind, "yes" if spec.directed else "no", spec.note)
    console.print(table)


@app.command()
def generate(
    name: str = typer.Argument(..., help="Dataset name from `datasets`."),
    cache_dir: Path = typer.Option(ds_mod.DEFAULT_CACHE, help="Where edge lists are cached."),
) -> None:
    """Materialise a dataset to Parquet (generate or download + cache)."""
    spec = ds_mod.resolve(name)
    with console.status(f"materialising {name} …"):
        path = spec.materialize(cache_dir)
    n_nodes, n_edges = ds_mod.graph_shape(path)
    console.print(f"[green]{name}[/green] → {path}  ({n_nodes:,} nodes, {n_edges:,} edges)")


@app.command()
def run(
    library: list[str] = typer.Option(None, "--library", "-l", help="Libraries (default: all)."),
    algo: list[str] = typer.Option(None, "--algo", "-a", help="Algorithms (default: all)."),
    dataset: list[str] = typer.Option(None, "--dataset", "-d", help="Datasets (default: er-1k)."),
    iters: int = typer.Option(5, help="Timed warm iterations (per round, per cell)."),
    warmup: int = typer.Option(1, help="Discarded warm iterations."),
    rounds: int = typer.Option(
        1,
        help="Interleaved measurement rounds. >1 rotates library order and pools "
        "samples across rounds to spread host drift fairly (mitigates VM noise).",
    ),
    threads: list[int] = typer.Option(
        [1],
        "--threads",
        "-t",
        help="Thread cap(s). Repeat to sweep, e.g. -t 1 -t 2 -t 4. 1 = apples-to-apples.",
    ),
    reference_max_nodes: int = typer.Option(
        50_000, help="Above this node count the oracle is skipped; correctness = n/a."
    ),
    timeout: float = typer.Option(600.0, help="Per-cell timeout (seconds)."),
    out: Path = typer.Option(DEFAULT_RESULTS, help="Results Parquet (rows are appended)."),
    cache_dir: Path = typer.Option(ds_mod.DEFAULT_CACHE, help="Dataset cache dir."),
    show: bool = typer.Option(True, help="Print the leaderboard after the run."),
) -> None:
    """Measure a (library × algorithm × dataset [× threads]) slice; write raw rows to Parquet.

    Every library is given the *same* thread budget at each level — those that
    parallelise (Ursa, rustworkx) speed up; those that don't (igraph's C core,
    NetworkX under the GIL) stay flat. That's a fair, generous comparison: the
    thread count is recorded on every row so a sweep becomes its own dimension.
    """
    cfg = RunConfig(
        libraries=library or known_libraries(),
        algorithms=algo or algos_mod.names(),
        datasets=dataset or ["er-1k"],
        iters=iters,
        warmup=warmup,
        rounds=rounds,
        threads=threads[0],
        reference_max_nodes=reference_max_nodes,
        timeout_s=timeout,
        cache_dir=cache_dir,
    )
    run_id = _run_id()
    per_level = len(cfg.libraries) * len(cfg.algorithms) * len(cfg.datasets)
    console.print(
        f"[bold]{run_id}[/bold]: {per_level * len(threads)} cells "
        f"({len(cfg.libraries)} libs × {len(cfg.algorithms)} algos × "
        f"{len(cfg.datasets)} datasets × threads {threads}), iters={iters}, rounds={rounds}"
    )
    for t in threads:
        cfg.threads = t
        with console.status(f"measuring … threads={t}"):
            run_matrix(cfg, out, run_id)
    console.print(f"[green]done[/green] → {out}")
    if show:
        report_mod.print_report(out, "warm", console)


@app.command()
def report(
    out: Path = typer.Option(DEFAULT_RESULTS, help="Results Parquet to read."),
    metric: str = typer.Option("warm", help="warm | cold | ingest | mem."),
    markdown: bool = typer.Option(False, help="Emit a Markdown leaderboard instead of a table."),
) -> None:
    """Render a leaderboard + gaps to-do list from a results Parquet."""
    if metric not in report_mod.METRICS:
        raise typer.BadParameter(f"metric must be one of {', '.join(report_mod.METRICS)}")
    if not Path(out).exists():
        raise typer.BadParameter(f"no results at {out}; run `run` first")
    if markdown:
        print(report_mod.markdown_leaderboard(out, metric))
    else:
        report_mod.print_report(out, metric, console)


@app.command()
def smoke(
    out: Path = typer.Option(
        None, help="Results Parquet (default: a temp file; nothing committed)."
    ),
) -> None:
    """Tiny end-to-end run for CI: prove the harness works and Ursa is correct.

    Exits non-zero if any Ursa cell fails to run or disagrees with the oracle.
    """
    import tempfile

    target = out or Path(tempfile.mkdtemp(prefix="ursa-bench-smoke-")) / "smoke.parquet"
    cfg = RunConfig(
        libraries=known_libraries(),
        # One of each shape: iterative (pagerank), adjacency (triangle_count,
        # clustering_coefficient), union-find (connected_components), trivial
        # (degree), the two all-pairs centralities whose direction/normalisation
        # conventions are easiest to get subtly wrong (betweenness, closeness),
        # and community detection scored by modularity (louvain, label_propagation).
        algorithms=[
            "pagerank",
            "triangle_count",
            "clustering_coefficient",
            "connected_components",
            "degree",
            "betweenness",
            "closeness",
            "louvain",
            "label_propagation",
            "pipeline_influencers",
        ],
        datasets=[ds_mod.SMOKE],
        iters=2,
        warmup=1,
        threads=1,
    )
    rows = run_matrix(cfg, target, _run_id())
    report_mod.print_report(target, "warm", console)

    failures = []
    for r in rows:
        if r.library != "ursa":
            continue
        if r.status != STATUS_OK:
            failures.append(f"{r.algorithm}: status={r.status} ({r.error})")
        elif r.correct is False:
            failures.append(f"{r.algorithm}: incorrect — {r.correctness_detail}")

    if failures:
        console.print("\n[red bold]SMOKE FAILED[/red bold]")
        for f in failures:
            console.print(f"  [red]{f}[/red]")
        raise typer.Exit(1)
    console.print("\n[green bold]smoke OK[/green bold]")


if __name__ == "__main__":
    app()
