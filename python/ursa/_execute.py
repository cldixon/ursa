"""Executing graph queries — Ursa's ``collect()``.

Thin Python orchestration: it turns a query into a small description (output
columns + a filter/sort/limit tail) and hands it to the Rust side
(``ursa._ursa.run_node_query``), which builds and executes **one** DataFusion
`LogicalPlan` — `Limit → Sort → Filter → GraphAlgorithmNode` — and returns one
Arrow batch. Two shapes funnel through the same path:

1. **A standalone algorithm** — ``ur.pagerank(edges).collect()`` (one column).
2. **A composed pipeline** — ``edges.nodes().with_columns(pr=ur.pagerank(edges),
   ...).filter(...).sort(...).head(n).collect()`` (many columns + tail).

Edges may be in-memory (``from_arrow`` / ``from_polars``) or a ``scan_edges``
Parquet/CSV file source. Anything outside the wired surface raises a clear error.
"""

from __future__ import annotations

import json
from typing import TYPE_CHECKING, Any

from ._result import MaterializedFrame

if TYPE_CHECKING:
    from ._expr import Expr
    from ._frames import EdgeFrame, NodeFrame

# Algorithms with a kernel wired into the execution path (mirrors
# ursa_plan::result::is_executable on the Rust side).
_EXECUTABLE = {
    "pagerank",
    "degree",
    "connected_components",
    "triangle_count",
    "clustering_coefficient",
    "neighbors_agg",
}

# Comparison operators supported in filters, with the operator that results from
# writing the comparison the other way round (literal on the left).
_FLIP = {">": "<", "<": ">", ">=": "<=", "<=": ">=", "==": "==", "!=": "!="}


# --- composed with_columns pipeline ----------------------------------------
# (Standalone `ur.pagerank(edges).collect()` promotes to a NodeFrame via
# GraphExpr and also lands here — see ursa._graph.GraphExpr.)
def collect_node_frame(frame: NodeFrame) -> MaterializedFrame:
    """Execute a composed ``edges.nodes().with_columns(...).filter/sort/head``."""
    graph_exprs: dict[str, Expr] | None = None
    filters: list[tuple[str, str, float]] = []
    sort: tuple[str, bool] | None = None
    limit: int | None = None

    for step in frame._plan:
        op = step.op
        if op in ("scan_edges", "scan_nodes", "nodes", "from_arrow", "from_polars", "rename"):
            continue  # source / metadata steps
        if op == "with_columns":
            if graph_exprs is not None:
                raise NotImplementedError(
                    "collect() supports a single with_columns step in a composed pipeline for now."
                )
            graph_exprs = step.args["exprs"]
        elif op == "filter":
            filters.append(_parse_filter(step.args["predicate"]))
        elif op == "sort":
            sort = _parse_sort(step.args)
        elif op == "head":
            limit = int(step.args["n"])
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step in a composed pipeline."
            )

    if not graph_exprs:
        raise NotImplementedError(
            "collect() on a NodeFrame needs a with_columns of graph algorithms "
            "(e.g. edges.nodes().with_columns(pr=ur.pagerank(edges)).collect())."
        )

    edges = _single_edges(graph_exprs.values())
    columns = [_algo_column(name, expr) for name, expr in graph_exprs.items()]
    # If this NodeFrame is a node attribute table, its columns join onto the algo
    # outputs by id (see run_node_query); edges.nodes()-derived frames have none.
    nodes = getattr(frame, "_attr_table", None)
    nodes_id = frame.id_col if nodes is not None else None
    return _run_query(edges, columns, filters, sort, limit, nodes, nodes_id)


# --- the one execution entry ------------------------------------------------
def _run_query(
    edges: EdgeFrame | None,
    columns: list[dict[str, Any]],
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None,
    limit: int | None,
    nodes: Any | None = None,
    nodes_id: str | None = None,
) -> MaterializedFrame:
    src, dst = _require_edges(edges)
    batch = _native().run_node_query(
        src, dst, json.dumps(columns), filters, sort, limit, nodes, nodes_id
    )
    return MaterializedFrame(batch)


def _algo_column(name: str, expr: Expr) -> dict[str, Any]:
    """Build the JSON IR for one output column from a graph expression."""
    verb = expr.payload["verb"]
    if verb not in _EXECUTABLE:
        raise NotImplementedError(
            f"the '{verb}' kernel is not wired into the execution path yet "
            f"(wired: {', '.join(sorted(_EXECUTABLE))})."
        )
    p = expr.payload
    column: dict[str, Any] = {"name": name, "kind": verb}
    if verb == "pagerank":
        column.update(
            damping=p.get("damping", 0.85),
            max_iter=p.get("max_iter", 30),
            tol=p.get("tol", 1e-6),
        )
    elif verb == "degree":
        column["direction"] = p.get("direction", "out")
    elif verb == "neighbors_agg":
        agg = p["agg"]
        operand = agg.payload.get("operand") if agg.kind == "agg" else None
        if agg.kind != "agg" or operand is None or operand.kind != "col":
            raise NotImplementedError(
                "neighbors().agg() supports ur.col(<name>).<fn>() with fn in "
                "mean/sum/min/max/count/n_unique over a numeric attribute column."
            )
        column.update(
            agg_fn=agg.payload["fn"],
            agg_column=operand.payload["name"],
            direction=p.get("direction", "out"),
        )
    return column


def _single_edges(exprs: Any) -> Any:
    """The one EdgeFrame shared by every graph expr, or an error if they differ."""
    edges = None
    for expr in exprs:
        if expr.kind != "graph" or "edges" not in expr.payload:
            raise NotImplementedError(
                "collect() supports with_columns of graph algorithms only; non-graph "
                "expressions are not executable yet."
            )
        e = expr.payload["edges"]
        if edges is None:
            edges = e
        elif e is not edges:
            raise NotImplementedError(
                "collect() requires every algorithm in with_columns to run over the "
                "same edges frame for now."
            )
    return edges


def _require_edges(edges: EdgeFrame | None) -> tuple[Any, Any]:
    """Resolve an EdgeFrame to (src, dst) int64 Arrow arrays, or raise clearly."""
    if edges is None:
        raise NotImplementedError("this expression is not associated with an edge frame.")

    arrays = getattr(edges, "_edge_arrays", None)
    if arrays is not None:
        return arrays

    scan = getattr(edges, "_scan_spec", None)
    if scan is not None:
        path = scan["path"]
        if not isinstance(path, str):
            raise NotImplementedError(
                "collect() over a scan_edges source supports a single string path "
                "(glob included) for now, not a list of paths."
            )
        batch = _native().scan_edges_arrow(path, scan["src"], scan["dst"])
        return batch.column(0), batch.column(1)

    raise NotImplementedError(
        "collect() needs edges from ur.from_arrow(...), ur.from_polars(...), or "
        "ur.scan_edges(<.parquet|.csv>); this frame has no resolvable source."
    )


def _parse_filter(predicate: Any) -> tuple[str, str, float]:
    """Lower a ``col <op> literal`` predicate to (column, op, value)."""
    unsupported = NotImplementedError(
        "collect() filters currently support a single `ur.col(name) <op> <number>` "
        "comparison (op in > >= < <= == !=); richer predicates are future work."
    )
    if predicate.kind != "binary" or predicate.payload["op"] not in _FLIP:
        raise unsupported

    op = predicate.payload["op"]
    left, right = predicate.payload["left"], predicate.payload["right"]
    if left.kind == "col" and right.kind == "lit":
        column, value = left.payload["name"], right.payload["value"]
    elif left.kind == "lit" and right.kind == "col":
        column, value, op = right.payload["name"], left.payload["value"], _FLIP[op]
    else:
        raise unsupported

    if not isinstance(value, (int, float)) or isinstance(value, bool):
        raise unsupported
    return (column, op, float(value))


def _parse_sort(args: dict[str, Any]) -> tuple[str, bool]:
    by = args["by"]
    if not isinstance(by, str):
        raise NotImplementedError(
            "collect() sort supports a single column name for now "
            "(e.g. .sort('pagerank', descending=True))."
        )
    return (by, bool(args.get("descending", False)))


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
