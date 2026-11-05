"""Write the Python library's results for the SQL engine's parity tests.

The SQL engine must give the same values as the Python library for the same
graph and parameters (cldixon/ursa#232, #235). This script runs each graph
function through the Python API on the bundled datasets and writes what it
returns to ``parity.json``, next to this file. ``ursa-engine/tests/parity.rs``
runs the same calls through SQL and compares ids, values and column types
exactly; floats are stored as the hex of their IEEE-754 bits, so the
comparison is bit for bit.

Run it from the repository root, after building the Python package:

    uv sync --reinstall-package ursa-graph
    uv run python ursa-engine/tests/fixtures/generate_parity.py

It rewrites ``parity.json`` from scratch each run; commit the result. Rerun it
when a kernel's output changes on purpose, or to add a case.
"""

from __future__ import annotations

import json
import struct
from pathlib import Path
from typing import Any

import ursa as ur

OUT = Path(__file__).with_name("parity.json")

# (dataset, edge file under python/ursa/datasets/data, loader)
DATASETS = [
    ("karate", "karate_edges.csv", ur.datasets.load_karate),  # integer ids
    ("florentine", "florentine_edges.csv", ur.datasets.load_florentine),  # string ids
]

# (function, keyword arguments): each runs as ``ur.<function>(edges, **kwargs)``
# and as ``SELECT * FROM <function>(g, name => value, ...)``.
CALLS: list[tuple[str, dict[str, Any]]] = [
    ("degree", {}),
    ("degree", {"direction": "in"}),
    ("degree", {"direction": "both"}),
    ("pagerank", {}),
    ("pagerank", {"direction": "both"}),
    ("pagerank", {"damping": 0.9, "direction": "both"}),
    ("pagerank", {"damping": 0.5, "max_iter": 100, "tol": 1e-10}),
    ("connected_components", {}),
    ("connected_components", {"mode": "strong"}),
    ("triangle_count", {}),
    ("louvain", {}),
    ("louvain", {"seed": 1}),
    ("louvain", {"resolution": 0.5, "seed": 7}),
]


def sql_literal(value: Any) -> str:
    """``value`` as a SQL literal."""
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    return repr(value)


def encode(value: Any) -> Any:
    """A float as the hex of its bits (exact); anything else as itself."""
    if isinstance(value, float):
        return struct.pack(">d", value).hex()
    return value


def case(dataset: str, csv: str, edges: Any, function: str, kwargs: dict[str, Any]) -> dict:
    table = getattr(ur, function)(edges, **kwargs).collect().to_arrow()
    assert table.column_names == ["id", function], table.column_names
    ids = table.column("id").to_pylist()
    values = table.column(function).to_pylist()
    return {
        "dataset": dataset,
        "csv": csv,
        "function": function,
        "sql_args": ", ".join(f"{k} => {sql_literal(v)}" for k, v in kwargs.items()),
        "id_type": str(table.schema.field("id").type),
        "value_type": str(table.schema.field(function).type),
        "rows": [[i, encode(v)] for i, v in zip(ids, values, strict=True)],
    }


def main() -> None:
    cases = []
    for dataset, csv, load in DATASETS:
        edges = load()
        for function, kwargs in CALLS:
            cases.append(case(dataset, csv, edges, function, kwargs))
    header = {
        "about": "Python library results for ursa-engine/tests/parity.rs; written by "
        "generate_parity.py (see its docstring). Floats are the hex of their IEEE-754 bits.",
        "ursa_version": ur.__version__,
    }
    # One case per line, so a diff shows which cases changed.
    lines = [f" {json.dumps(k)}: {json.dumps(v)}," for k, v in header.items()]
    body = ",\n".join(f"  {json.dumps(c)}" for c in cases)
    OUT.write_text("{\n" + "\n".join(lines) + '\n "cases": [\n' + body + "\n ]\n}\n")
    print(f"wrote {len(cases)} cases to {OUT}")


if __name__ == "__main__":
    main()
