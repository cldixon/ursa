"""Executing graph queries — the walking-skeleton ``collect()``.

Thin Python orchestration over the real Rust execution path
(``ursa_plan`` behind ``ursa._ursa.*``). Two shapes are wired end-to-end:

1. **A standalone node-valued algorithm** — ``ur.pagerank(edges).collect()``.
2. **A composed pipeline** — ``edges.nodes().with_columns(pr=ur.pagerank(edges),
   ...).filter(...).sort(...).head(n).collect()``: every algorithm over the same
   edges shares one ``IdMap`` (hence one id ordering), so results are assembled
   into an aligned ``(id, value...)`` table, and the ``filter``/``sort``/``head``
   tail runs through a DataFusion ``DataFrame`` (``run_relational``).

Edges may be in-memory (``from_arrow`` / ``from_polars``) or a ``scan_edges``
Parquet/CSV file source (read through a DataFusion scan). Anything outside this
surface raises a clear, honest error rather than pretending.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from ._result import MaterializedFrame

if TYPE_CHECKING:
    from ._expr import Expr
    from ._frames import EdgeFrame, NodeFrame

# Algorithms with a kernel wired into the execution path (mirrors
# ursa_plan::result::is_executable on the Rust side).
_EXECUTABLE = {"pagerank", "degree", "connected_components", "triangle_count"}

# Comparison operators supported in composed-pipeline filters, with the operator
# that results from writing the comparison the other way round (literal on left).
_FLIP = {">": "<", "<": ">", ">=": "<=", "<=": ">=", "==": "==", "!=": "!="}


# --- standalone node-valued algorithm --------------------------------------
def collect_graph_expr(expr: Expr) -> MaterializedFrame:
    """Execute a standalone node-valued graph expression and materialize it."""
    if expr.kind != "graph":
        raise NotImplementedError(
            "collect() on a bare expression is wired only for standalone node-valued "
            "graph algorithms (e.g. ur.pagerank(edges).collect())."
        )
    src, dst = _require_edges(expr.payload.get("edges"))
    return MaterializedFrame(_run_algo(expr, src, dst))


# --- composed with_columns pipeline ----------------------------------------
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

    # All algorithms must run over the same edges (so their id orderings align).
    edges = _single_edges(graph_exprs.values())
    src, dst = _require_edges(edges)

    import pyarrow as pa

    native = _native()
    id_col: Any = None
    columns: dict[str, Any] = {}
    for name, expr in graph_exprs.items():
        batch = _run_algo(expr, src, dst)
        if id_col is None:
            id_col = batch.column(0)
        elif not batch.column(0).equals(id_col):
            raise AssertionError("algorithm id columns are misaligned (different edge sets?)")
        columns[name] = batch.column(1)

    table = pa.record_batch({"id": id_col, **columns})
    result = native.run_relational(table, filters, sort, limit)
    return MaterializedFrame(result)


# --- helpers ----------------------------------------------------------------
def _run_algo(expr: Expr, src: Any, dst: Any) -> Any:
    """Run one node-valued graph algorithm, returning its ``(id, value)`` batch."""
    verb = expr.payload["verb"]
    if verb not in _EXECUTABLE:
        raise NotImplementedError(
            f"the '{verb}' kernel is not wired into the execution path yet "
            f"(wired: {', '.join(sorted(_EXECUTABLE))})."
        )
    native = _native()
    p = expr.payload
    if verb == "pagerank":
        return native.run_pagerank(
            src, dst, p.get("damping", 0.85), p.get("max_iter", 30), p.get("tol", 1e-6)
        )
    if verb == "degree":
        return native.run_degree(src, dst, p.get("direction", "out"))
    if verb == "connected_components":
        return native.run_connected_components(src, dst)
    return native.run_triangle_count(src, dst)


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
