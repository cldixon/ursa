"""Render a results Parquet into human-facing views — a *separate* step.

Nothing here runs a benchmark; it only reads the Parquet a run produced. That
separation is the point: the same results file can be re-rendered any number of
ways, diffed across commits, or fed to new tooling, without re-measuring.

Two views:

* a **leaderboard** — one metric (warm kernel time, cold end-to-end, or peak
  memory) pivoted with (dataset, algorithm) down the side and libraries across
  the top, each ``ok`` cell annotated with its speed relative to Ursa;
* a **gaps** view — the flywheel's to-do list: ``unsupported`` cells (feature
  gaps), ``incorrect`` cells (correctness bugs), ``error`` cells (crashes/OOM),
  and ``ok`` cells where Ursa trails the field by a wide margin (perf gaps).
"""

from __future__ import annotations

import math
from pathlib import Path

from rich.console import Console
from rich.table import Table

from .results import read_results

BASELINE = "ursa"  # the library everything is measured relative to

# metric key -> (human label, lower_is_better, formatter name). The value for a
# row is produced by _metric_value (some metrics are derived, not raw columns).
METRICS = {
    "warm": ("warm kernel (median)", True, "secs"),
    "cold": ("cold end-to-end", True, "secs"),
    "ingest": ("ingest", True, "secs"),
    "mem": ("graph memory (peak − baseline)", True, "bytes"),
    "memedge": ("memory / edge", True, "bytes"),
}


def _metric_value(row: dict, metric: str) -> float:
    """Extract a metric's value from a result row, deriving where needed.

    ``mem`` isolates the graph+compute footprint (peak RSS minus the interpreter
    baseline captured before the library loaded); ``memedge`` divides that by edge
    count — the number that actually distinguishes a CSR from a dict-of-dicts.
    """
    if metric == "warm":
        return row["compute_warm_median_s"]
    if metric == "cold":
        return row["end_to_end_cold_s"]
    if metric == "ingest":
        return row["ingest_s"]
    if metric in ("mem", "memedge"):
        peak, base = row.get("peak_rss_bytes"), row.get("baseline_rss_bytes")
        if peak is None or base is None or peak < 0 or base < 0:
            return float("nan")
        graph_mem = peak - base
        if metric == "memedge":
            e = row.get("n_edges") or 0
            return graph_mem / e if e else float("nan")
        return graph_mem
    return float("nan")


def _fmt_secs(x: float) -> str:
    if x is None or (isinstance(x, float) and math.isnan(x)):
        return "—"
    if x < 1e-3:
        return f"{x * 1e6:.0f}µs"
    if x < 1.0:
        return f"{x * 1e3:.1f}ms"
    return f"{x:.3f}s"


def _fmt_bytes(x: int) -> str:
    if x is None or x < 0:
        return "—"
    v = float(x)
    for unit in ("B", "KB", "MB", "GB"):
        if v < 1024 or unit == "GB":
            return f"{v:.0f}{unit}" if unit == "B" else f"{v:.1f}{unit}"
        v /= 1024
    return f"{v:.1f}GB"


def _load(path: str | Path) -> list[dict]:
    return read_results(path).to_pylist()


def _pivot(rows: list[dict]) -> dict[tuple[str, str], dict[str, dict]]:
    """(dataset, algorithm) -> {library -> row}."""
    grid: dict[tuple[str, str], dict[str, dict]] = {}
    for r in rows:
        grid.setdefault((r["dataset"], r["algorithm"]), {})[r["library"]] = r
    return grid


def _cell(row: dict, metric: str, formatter: str, baseline_val: float | None) -> str:
    status = row["status"]
    if status == "unsupported":
        return "[dim]n/a[/dim]"
    if status == "error":
        return "[red]err[/red]"
    val = _metric_value(row, metric)
    missing = val is None or (isinstance(val, float) and math.isnan(val))
    if missing or (formatter == "bytes" and val < 0):
        return "—"
    text = _fmt_bytes(val) if formatter == "bytes" else _fmt_secs(val)
    if status == "incorrect":
        text = f"[red]✗[/red] {text}"
    # Annotate speed relative to the baseline (Ursa) for time metrics.
    if baseline_val and formatter == "secs" and val > 0 and not math.isnan(baseline_val):
        ratio = val / baseline_val
        if abs(ratio - 1.0) > 0.05:
            text += f" [dim]({ratio:.1f}×)[/dim]"
    return text


def leaderboard_table(path: str | Path, metric: str) -> Table:
    label, _lower, formatter = METRICS[metric]
    rows = _load(path)
    grid = _pivot(rows)
    libraries = sorted({r["library"] for r in rows}, key=lambda x: (x != BASELINE, x))

    table = Table(title=f"Ursa benchmark — {label} (relative to {BASELINE})", header_style="bold")
    table.add_column("dataset")
    table.add_column("algorithm")
    for lib in libraries:
        table.add_column(lib, justify="right")

    for dataset, algo in sorted(grid):
        cells = grid[(dataset, algo)]
        base_row = cells.get(BASELINE)
        base_val = None
        if base_row is not None and base_row["status"] in {"ok", "incorrect"}:
            bv = _metric_value(base_row, metric)
            base_val = bv if not (isinstance(bv, float) and math.isnan(bv)) else None
        line = [dataset, algo]
        for lib in libraries:
            row = cells.get(lib)
            line.append(_cell(row, metric, formatter, base_val) if row else "—")
        table.add_row(*line)
    return table


def gaps(path: str | Path, slow_factor: float = 3.0) -> list[str]:
    """The flywheel's to-do list, as human-readable lines."""
    rows = _load(path)
    grid = _pivot(rows)
    out: list[str] = []

    for r in rows:
        if r["library"] == BASELINE and r["status"] == "unsupported":
            out.append(f"FEATURE  {r['algorithm']}: Ursa has no implementation")
        if r["status"] == "incorrect":
            out.append(
                f"CORRECT  {r['library']} {r['algorithm']} on {r['dataset']}: "
                f"{r['correctness_detail']}"
            )

    # Perf gaps: Ursa's warm kernel materially slower than the fastest competitor.
    for (dataset, algo), cells in grid.items():
        base = cells.get(BASELINE)
        if base is None or base["status"] not in {"ok", "incorrect"}:
            continue
        bv = base["compute_warm_median_s"]
        if isinstance(bv, float) and math.isnan(bv):
            continue

        def _valid(c: dict) -> bool:
            v = c["compute_warm_median_s"]
            return c["status"] in {"ok", "incorrect"} and not (
                isinstance(v, float) and math.isnan(v)
            )

        rivals = [
            (lib, c["compute_warm_median_s"])
            for lib, c in cells.items()
            if lib != BASELINE and _valid(c)
        ]
        if not rivals:
            continue
        best_lib, best = min(rivals, key=lambda t: t[1])
        if best > 0 and bv / best >= slow_factor:
            out.append(
                f"PERF     {algo} on {dataset}: Ursa {_fmt_secs(bv)} vs "
                f"{best_lib} {_fmt_secs(best)} ({bv / best:.1f}× slower)"
            )

    # Deduplicate FEATURE lines (they repeat per dataset).
    seen: set[str] = set()
    deduped = []
    for line in out:
        if line.startswith("FEATURE"):
            if line in seen:
                continue
            seen.add(line)
        deduped.append(line)
    return sorted(deduped)


def markdown_leaderboard(path: str | Path, metric: str) -> str:
    """A committable Markdown table for the docs/leaderboard page."""
    label, _lower, formatter = METRICS[metric]
    rows = _load(path)
    grid = _pivot(rows)
    libraries = sorted({r["library"] for r in rows}, key=lambda x: (x != BASELINE, x))

    def plain(row: dict | None) -> str:
        if row is None:
            return "—"
        if row["status"] == "unsupported":
            return "n/a"
        if row["status"] == "error":
            return "err"
        val = _metric_value(row, metric)
        if val is None or (isinstance(val, float) and math.isnan(val)):
            return "—"
        text = _fmt_bytes(val) if formatter == "bytes" else _fmt_secs(val)
        return ("✗ " + text) if row["status"] == "incorrect" else text

    lines = [f"### {label}", "", "| dataset | algorithm | " + " | ".join(libraries) + " |"]
    lines.append("|" + "---|" * (2 + len(libraries)))
    for dataset, algo in sorted(grid):
        cells = grid[(dataset, algo)]
        row_cells = [plain(cells.get(lib)) for lib in libraries]
        lines.append(f"| {dataset} | {algo} | " + " | ".join(row_cells) + " |")
    return "\n".join(lines)


def print_report(path: str | Path, metric: str, console: Console | None = None) -> None:
    console = console or Console()
    console.print(leaderboard_table(path, metric))
    gap_lines = gaps(path)
    if gap_lines:
        console.print("\n[bold]Gaps (flywheel to-do):[/bold]")
        for line in gap_lines:
            console.print(f"  {line}")
    else:
        console.print("\n[green]No gaps surfaced in this run.[/green]")
