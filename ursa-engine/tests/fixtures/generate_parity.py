"""Write the Python library's results for the SQL engine's parity tests.

The SQL engine must give the same values as the Python library for the same
graph and parameters (cldixon/ursa#232, #235, #236). This script runs each
graph function through the Python API on the bundled datasets and writes what
it returns to ``parity.json``, next to this file. ``ursa-engine/tests/parity.rs``
runs the same calls through SQL and compares column names, column types, ids
and values exactly; floats are stored as the hex of their IEEE-754 bits (16
digits for ``double``, 8 for ``float``), so the comparison is bit for bit.

Run it from the repository root, after building the Python package:

    uv sync --reinstall-package ursa-graph
    uv run python ursa-engine/tests/fixtures/generate_parity.py

It rewrites ``parity.json`` from scratch each run; commit the result. Rerun it
when a kernel's output changes on purpose, or to add a case.
"""

from __future__ import annotations

import json
import struct
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pyarrow as pa

import ursa as ur
from ursa import _graph

# The layouts are held back from Python's public API (see ``ursa/__init__.py``)
# but live in ``ursa._graph``; the SQL engine exposes them, so they are checked
# against the same kernels here.

OUT = Path(__file__).with_name("parity.json")

# (dataset, edge file under python/ursa/datasets/data, loader)
DATASETS = [
    ("karate", "karate_edges.csv", ur.datasets.load_karate),  # integer ids
    ("florentine", "florentine_edges.csv", ur.datasets.load_florentine),  # string ids
    ("lesmis", "lesmis_edges.csv", ur.datasets.load_lesmis),  # string ids, a weight column
]

# Weight expressions, as SQL writes them and as the Python API builds them.
WEIGHTS: dict[str, Callable[[], Any]] = {
    "weight": lambda: ur.col("weight"),
    "weight * 2 + 1": lambda: ur.col("weight") * 2 + 1,
    "weight * 0.5": lambda: ur.col("weight") * 0.5,
}

# The Python function each SQL function runs as. Statistics are asked for their
# one-row frame (``lazy=True``) rather than the plain number.
STATISTICS = {"density", "avg_path_length", "diameter"}
LAYOUTS = {"layout_fr", "layout_random", "layout_circle"}

# (function, keyword arguments): each runs as ``ur.<function>(edges, **kwargs)``
# and as ``SELECT * FROM <function>(g, name => value, ...)``. A ``weight`` is a
# key of WEIGHTS.
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
    ("clustering_coefficient", {}),
    ("closeness", {}),
    ("closeness", {"direction": "in"}),
    ("closeness", {"direction": "both"}),
    ("betweenness", {}),
    ("betweenness", {"direction": "both"}),
    ("betweenness", {"sample": 0.5, "seed": 3}),
    ("label_propagation", {}),
    ("label_propagation", {"max_iter": 5, "seed": 2}),
    ("layout_fr", {"seed": 1}),
    ("layout_fr", {"iterations": 50, "k": 2.0, "gravity": 0.1, "seed": 5}),
    ("layout_random", {}),
    ("layout_random", {"k": 3.0, "seed": 9}),
    ("layout_circle", {}),
    ("layout_circle", {"k": 2.5}),
    ("density", {}),
    ("density", {"direction": "both"}),
    ("avg_path_length", {}),
    ("avg_path_length", {"direction": "both"}),
    ("avg_path_length", {"sample": 0.5, "direction": "in"}),
    ("diameter", {}),
    ("diameter", {"approximate": False, "direction": "both"}),
    ("describe", {}),
    ("describe", {"full": True, "direction": "both"}),
    ("describe", {"full": True, "nodes": "touched"}),
]

# Weighted calls, on the datasets with a ``weight`` column.
WEIGHTED_CALLS: list[tuple[str, dict[str, Any]]] = [
    ("pagerank", {"weight": "weight"}),
    ("pagerank", {"weight": "weight", "direction": "both"}),
    ("pagerank", {"damping": 0.9, "weight": "weight * 0.5"}),
    ("closeness", {"weight": "weight"}),
    ("closeness", {"weight": "weight * 2 + 1", "direction": "both"}),
    ("betweenness", {"weight": "weight"}),
    ("betweenness", {"weight": "weight", "direction": "both"}),
    ("louvain", {"weight": "weight"}),
    ("louvain", {"weight": "weight * 2 + 1", "seed": 7}),
]
WEIGHTED_DATASETS = {"lesmis"}


def sql_literal(value: Any) -> str:
    """``value`` as a SQL literal."""
    if isinstance(value, bool):
        return "true" if value else "false"
    if isinstance(value, str):
        return "'" + value.replace("'", "''") + "'"
    return repr(value)


def sql_args(kwargs: dict[str, Any]) -> str:
    """The named arguments as SQL writes them; a weight is an expression."""
    return ", ".join(
        f"{k} => {v}" if k == "weight" else f"{k} => {sql_literal(v)}" for k, v in kwargs.items()
    )


def run(function: str, edges: Any, kwargs: dict[str, Any]) -> pa.Table:
    """The Python API's result for ``function`` over ``edges``."""
    kwargs = {k: WEIGHTS[v]() if k == "weight" else v for k, v in kwargs.items()}
    if function in LAYOUTS:
        frame = getattr(_graph, function)(edges, **kwargs)
    elif function in STATISTICS:
        frame = getattr(ur, function)(edges, **kwargs, lazy=True)
    else:
        frame = getattr(ur, function)(edges, **kwargs)
    return frame.collect().to_arrow()


def encode(value: Any, type_: pa.DataType) -> Any:
    """A float as the hex of its bits (exact); anything else as itself."""
    if value is None:
        return None
    if pa.types.is_float64(type_):
        return struct.pack(">d", value).hex()
    if pa.types.is_float32(type_):
        return struct.pack(">f", value).hex()
    return value


def case(dataset: str, csv: str, edges: Any, function: str, kwargs: dict[str, Any]) -> dict:
    table = run(function, edges, kwargs)
    types = [field.type for field in table.schema]
    columns = [table.column(i).to_pylist() for i in range(table.num_columns)]
    return {
        "dataset": dataset,
        "csv": csv,
        "function": function,
        "sql_args": sql_args(kwargs),
        "columns": [[field.name, str(field.type)] for field in table.schema],
        "rows": [
            [encode(v, t) for v, t in zip(row, types, strict=True)]
            for row in zip(*columns, strict=True)
        ],
    }


def main() -> None:
    cases = []
    for dataset, csv, load in DATASETS:
        edges = load()
        calls = CALLS + (WEIGHTED_CALLS if dataset in WEIGHTED_DATASETS else [])
        for function, kwargs in calls:
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
