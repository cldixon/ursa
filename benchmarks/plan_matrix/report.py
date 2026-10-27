"""Render `results.tsv` as a markdown table of speedups against a baseline label.

    python report.py results.tsv --base main --base main.r2 --vs branch branch.r2

The baseline is the faster of the `--base` runs per scenario; the "noise" column is the
spread between two base runs. Each `--vs` column is base time / candidate time, so >1 is
faster; cells outside 0.87x-1.15x are bold. Read the noise column before believing any
bold cell: on a shared VM a single scenario can swing 20% between identical runs.
"""

from __future__ import annotations

import argparse
import collections

p = argparse.ArgumentParser()
p.add_argument("results")
p.add_argument("--base", action="append", default=[])
p.add_argument("--vs", nargs="+", required=True)
a = p.parse_args()
base_labels = a.base or ["main"]

rows: dict[str, dict[str, float]] = collections.defaultdict(dict)
with open(a.results) as f:
    for line in f:
        parts = line.rstrip("\n").split("\t")
        if len(parts) == 4 and parts[2] != "FAIL":
            rows[parts[1]][parts[0]] = float(parts[2])

print("| scenario | " + " | ".join(base_labels) + " | noise | " + " | ".join(a.vs) + " |")
print("|---" * (len(base_labels) + len(a.vs) + 2) + "|")
for scenario, times in rows.items():
    bases = [times[b] for b in base_labels if b in times]
    if not bases:
        continue
    base = min(bases)
    noise = f"{(max(bases) - base) / base * 100:.0f}%" if len(bases) > 1 else "—"

    def cell(label: str, times: dict[str, float] = times, base: float = base) -> str:
        if label not in times:
            return "—"
        r = base / times[label]
        return f"**{r:.2f}×**" if r >= 1.15 or r <= 0.87 else f"{r:.2f}×"

    shown = " | ".join(f"{times[b]:.4g}" if b in times else "—" for b in base_labels)
    print(f"| {scenario} | {shown} | {noise} | " + " | ".join(cell(v) for v in a.vs) + " |")
