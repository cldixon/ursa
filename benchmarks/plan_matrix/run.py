"""Run every scenario against the `ursa` the given interpreter imports, appending rows.

    python run.py --label main --python .venv-main/bin/python --data data --out results.tsv

Each scenario runs in its own process (`scenarios.py`), so peak RSS is per scenario and
no warm state carries over. Rows are `label<TAB>scenario<TAB>seconds<TAB>peak_mb`; a
scenario that fails or times out records `FAIL`. To compare two builds, install each
into its own environment and run this once per environment with a different `--label`.
Run the baseline twice (`main`, `main.r2`) to get the noise floor `report.py` prints.
"""

from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent

p = argparse.ArgumentParser()
p.add_argument("--label", required=True)
p.add_argument("--python", default=sys.executable)
p.add_argument("--data", default=str(HERE / "data"))
p.add_argument("--out", default=str(HERE / "results.tsv"))
p.add_argument("--only", nargs="*", help="a subset of scenario names")
p.add_argument("--timeout", type=int, default=600)
a = p.parse_args()

script = str(HERE / "scenarios.py")
# The scenario list comes from the interpreter under test, which has `ursa`; this
# runner itself needs only the standard library.
listed = subprocess.run([a.python, "-I", script, "--list"], capture_output=True, text=True, cwd="/")
names = a.only or listed.stdout.split()
if not names:
    sys.exit(f"no scenarios: {listed.stderr.strip()}")

with open(a.out, "a") as out:
    for name in names:
        try:
            r = subprocess.run(
                [a.python, "-I", script, str(Path(a.data).resolve()), name],
                capture_output=True,
                text=True,
                timeout=a.timeout,
                cwd="/",  # never import an `ursa` checkout from the cwd
            )
            row = r.stdout.strip().splitlines()[-1] if r.returncode == 0 else f"{name}\tFAIL\t-"
        except subprocess.TimeoutExpired:
            row = f"{name}\tFAIL\t-"
        line = f"{a.label}\t{row}"
        print(line, flush=True)
        out.write(line + "\n")
