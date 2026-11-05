"""Write the Python library's results for the SQL engine's parity tests.

The SQL engine must give the same values as the Python library for the same
graph and parameters (cldixon/ursa#232, #235, #236). This script runs each
graph function through the Python API on the bundled datasets and writes what
it returns to ``parity.json``, next to this file. ``ursa-engine/tests/parity.rs``
runs the same calls through SQL and compares column names, column types, ids
and values exactly; floats are stored as the hex of their IEEE-754 bits (16
digits for ``double``, 8 for ``float``), so the comparison is bit for bit.

The traversals (``hop``, ``shortest_path``, ``random_walk``, #238) are cases
too, with their rows in the Python library's order, and the file records the
Python signatures of the traversals so the SQL engine's specs of them
(``ursa-engine/src/graph/traversal.rs``) are held to the same names and
defaults.

Run it from the repository root, after building the Python package:

    uv sync --reinstall-package ursa-graph
    uv run python ursa-engine/tests/fixtures/generate_parity.py

It rewrites ``parity.json`` from scratch each run; commit the result. Rerun it
when a kernel's output changes on purpose, or to add a case.
"""

from __future__ import annotations

import inspect
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


# --- traversals ---------------------------------------------------------------
# Each traversal case runs as ``SELECT * FROM <function>(<dataset>, <sql_args>)``
# and as the Python call beside it. Their rows are compared in order: SQL
# returns a traversal's rows in the order the Python library does.

# Per dataset: two seeds, an id not in the graph, and two path endpoints.
NODES: dict[str, dict[str, Any]] = {
    "karate": {"seeds": [0, 33], "missing": 99, "path": (0, 26)},
    "florentine": {
        "seeds": ["Medici", "Strozzi"],
        "missing": "Sforza",
        "path": ("Acciaiuoli", "Strozzi"),
    },
    "lesmis": {
        "seeds": ["Valjean", "Marius"],
        "missing": "Javert2",
        "path": ("Napoleon", "Marius"),
    },
}

# The subquery SQL gives for Python's seed NodeFrame on karate; ``{data}`` is
# the dataset directory, filled in by parity.rs.
OFFICERS_SQL = "(SELECT id FROM '{data}/karate_nodes.csv' WHERE club = 'Officer')"


def sql_list(values: list[Any]) -> str:
    """``values`` as a SQL list literal."""
    return "[" + ", ".join(sql_literal(v) for v in values) + "]"


def hop(n: int, direction: str, seeds: Any) -> Callable[[Any], Any]:
    return lambda edges: ur.hop(edges, n=n, direction=direction).from_(seeds)


def traversal_cases(dataset: str) -> list[tuple[str, str, Callable[[Any], Any]]]:
    """``(function, sql_args, python call over the edges)`` for ``dataset``."""
    ids = NODES[dataset]
    seeds, missing, (source, target) = ids["seeds"], ids["missing"], ids["path"]
    cases: list[tuple[str, str, Callable[[Any], Any]]] = []
    for n in (1, 2):
        for direction in ("out", "in", "both"):
            cases.append(
                (
                    "hop",
                    f"seeds => {sql_list(seeds)}, n => {n}, direction => '{direction}'",
                    hop(n, direction, seeds),
                )
            )
    # One id; an id not in the graph and a repeated seed (#225: an unknown
    # seed reaches nothing, a repeated one repeats its rows); no seeds.
    cases.append(("hop", f"seeds => {sql_literal(seeds[0])}", hop(1, "out", [seeds[0]])))
    repeated = [seeds[0], missing, seeds[0]]
    cases.append(
        (
            "hop",
            f"seeds => {sql_list(repeated)}, direction => 'both'",
            hop(1, "both", repeated),
        )
    )
    cases.append(("hop", "seeds => []", hop(1, "out", [])))
    if dataset == "karate":
        _, members = ur.datasets.load_karate(with_nodes=True)
        officers = members.filter(ur.col("club") == "Officer")
        cases.append(("hop", f"seeds => {OFFICERS_SQL}, n => 2", hop(2, "out", officers)))

    for direction in ("out", "in", "both"):
        cases.append(
            (
                "shortest_path",
                f"source => {sql_literal(source)}, target => {sql_literal(target)}, "
                f"direction => '{direction}'",
                lambda edges, d=direction: ur.shortest_path(edges, source, target, direction=d),
            )
        )
    # Back along the path: often no path one way.
    cases.append(
        (
            "shortest_path",
            f"source => {sql_literal(target)}, target => {sql_literal(source)}",
            lambda edges: ur.shortest_path(edges, target, source),
        )
    )
    cases.append(
        (
            "shortest_path",
            f"source => {sql_literal(source)}, target => {sql_literal(missing)}",
            lambda edges: ur.shortest_path(edges, source, missing),
        )
    )
    if dataset in WEIGHTED_DATASETS:
        for weight, direction in (("weight", "out"), ("weight * 2 + 1", "both")):
            cases.append(
                (
                    "shortest_path",
                    f"source => {sql_literal(source)}, target => {sql_literal(target)}, "
                    f"weight => {weight}, direction => '{direction}'",
                    lambda edges, w=weight, d=direction: ur.shortest_path(
                        edges, source, target, weight=WEIGHTS[w](), direction=d
                    ),
                )
            )

    starts = [*seeds, missing]
    cases.append(
        (
            "random_walk",
            f"start => {sql_list(starts)}, steps => 4, walks_per_node => 2, seed => 7",
            lambda edges: ur.random_walk(edges, starts, 4, walks_per_node=2, seed=7),
        )
    )
    cases.append(
        (
            "random_walk",
            f"start => {sql_literal(seeds[1])}, steps => 10",
            lambda edges: ur.random_walk(edges, [seeds[1]], 10),
        )
    )
    return cases


def signatures() -> dict[str, list[list[Any]]]:
    """Each traversal's parameters besides the edges, as ``[name, default]``,
    the node ids first as SQL orders them; a required parameter's default is
    ``"<required>"``. ``hop``'s seeds are ``.from_(seeds)``'s."""

    def params(function: Callable[..., Any], skip: int) -> list[list[Any]]:
        out = []
        for p in list(inspect.signature(function).parameters.values())[skip:]:
            default = "<required>" if p.default is inspect.Parameter.empty else p.default
            out.append([p.name, default])
        return out

    return {
        "hop": params(_graph._Hop.from_, 1) + params(ur.hop, 1),
        "shortest_path": params(ur.shortest_path, 1),
        "random_walk": params(ur.random_walk, 1),
    }


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
    return result(dataset, csv, function, sql_args(kwargs), run(function, edges, kwargs))


def result(dataset: str, csv: str, function: str, args: str, table: pa.Table) -> dict:
    types = [field.type for field in table.schema]
    columns = [table.column(i).to_pylist() for i in range(table.num_columns)]
    return {
        "dataset": dataset,
        "csv": csv,
        "function": function,
        "sql_args": args,
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
        for function, args, call in traversal_cases(dataset):
            table = call(edges).collect().to_arrow()
            cases.append(result(dataset, csv, function, args, table))
    header = {
        "about": "Python library results for ursa-engine/tests/parity.rs; written by "
        "generate_parity.py (see its docstring). Floats are the hex of their IEEE-754 bits.",
        "ursa_version": ur.__version__,
        "traversal_signatures": signatures(),
    }
    # One case per line, so a diff shows which cases changed.
    lines = [f" {json.dumps(k)}: {json.dumps(v)}," for k, v in header.items()]
    body = ",\n".join(f"  {json.dumps(c)}" for c in cases)
    OUT.write_text("{\n" + "\n".join(lines) + '\n "cases": [\n' + body + "\n ]\n}\n")
    print(f"wrote {len(cases)} cases to {OUT}")


if __name__ == "__main__":
    main()
