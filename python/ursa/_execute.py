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
import threading
from typing import TYPE_CHECKING, Any

from ._result import MaterializedFrame

if TYPE_CHECKING:
    from ._expr import Expr
    from ._frames import EdgeFrame, NodeFrame

# Guards the per-frame lazy index build so concurrent first-collects over one
# frame share a single build (the spec's "concurrent queries share one build");
# collect() runs GIL-free, so the memo assignment itself needs guarding.
_INDEX_BUILD_LOCK = threading.Lock()

# Algorithms with a kernel wired into the execution path (mirrors
# ursa_plan::result::is_executable on the Rust side).
_EXECUTABLE = {
    "pagerank",
    "degree",
    "connected_components",
    "triangle_count",
    "clustering_coefficient",
    "closeness",
    "betweenness",
    "label_propagation",
    "louvain",
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
    # describe() is a whole-graph summary, computed eagerly off the topology and
    # wrapped as a one-row frame (its own branch — not a per-node algorithm).
    for step in frame._plan:
        if step.op == "describe":
            index = _require_index(step.args["edges"])
            batch = _native().graph_describe(index, bool(step.args.get("full", False)))
            return MaterializedFrame(batch)

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
    nodes = _resolve_node_attr_table(frame)
    nodes_id = frame.id_col if nodes is not None else None
    return _run_query(edges, columns, filters, sort, limit, nodes, nodes_id)


def _scan_storage_options(scan: dict[str, Any]) -> dict[str, str] | None:
    """The storage_options for a scan spec, after rejecting the unsupported
    ``store=`` (obstore) parameter. ``s3://``/``gs://``/``az://`` credentials and
    config flow through storage_options; obstore interop is future work."""
    if scan.get("store") is not None:
        raise NotImplementedError(
            "store= (a pre-configured obstore store) is not supported yet; pass "
            "storage_options={...} instead."
        )
    return scan.get("storage_options")


def _resolve_node_attr_table(frame: NodeFrame) -> Any | None:
    """The node attribute table as a RecordBatch: an in-memory ``from_arrow`` table
    if present, else a ``scan_nodes`` file source materialized through a DataFusion
    scan. ``edges.nodes()``-derived frames have neither and return ``None``."""
    inmem = getattr(frame, "_attr_table", None)
    if inmem is not None:
        return inmem
    scan = getattr(frame, "_scan_spec", None)
    if scan is not None:
        path = scan["path"]
        if not isinstance(path, str):
            raise NotImplementedError(
                "collect() over a scan_nodes source supports a single string path "
                "(glob included) for now, not a list of paths."
            )
        return _native().scan_nodes_arrow(path, scan["id"], _scan_storage_options(scan))
    return None


# --- frame-positioned traversals (ur.hop / ur.shortest_path) ----------------
def collect_edge_frame(frame: EdgeFrame) -> MaterializedFrame:
    """Execute a traversal EdgeFrame — ``ur.hop(edges, n).from_(seeds)`` or
    ``ur.shortest_path(edges, s, t)`` — plus an optional
    ``filter``/``sort``/``head``/``distinct`` tail."""
    traversal = None
    filters: list[tuple[str, str, float]] = []
    sort: tuple[str, bool] | None = None
    limit: int | None = None
    distinct = False

    for step in frame._plan:
        op = step.op
        if op in ("hop", "shortest_path"):
            traversal = step
        elif op in (
            "scan_edges",
            "from_arrow",
            "from_polars",
            "nodes",
            "reverse",
            "select",
            "rename",
        ):
            continue  # source / metadata steps
        elif op == "filter":
            filters.append(_parse_filter(step.args["predicate"]))
        elif op == "sort":
            sort = _parse_sort(step.args)
        elif op == "head":
            limit = int(step.args["n"])
        elif op == "distinct":
            distinct = True
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step after a traversal."
            )

    if traversal is None:
        raise NotImplementedError(
            "collect() on an EdgeFrame is supported for traversals (ur.hop, "
            "ur.shortest_path) in v0.1; to compute metrics, call an algorithm on it "
            "(e.g. ur.pagerank(edges))."
        )

    index = _require_index(traversal.args["edges"])
    if traversal.op == "hop":
        seeds = _resolve_seeds(traversal.args["seeds"])
        batch = _native().run_hop_query(
            index,
            seeds,
            int(traversal.args["n"]),
            traversal.args["direction"],
            filters,
            sort,
            limit,
            distinct,
        )
    else:  # shortest_path
        if traversal.args.get("weight") is not None:
            raise NotImplementedError(
                "weighted shortest_path is not supported yet; omit weight= for unweighted BFS."
            )
        batch = _native().run_path_query(
            index,
            int(traversal.args["source"]),
            int(traversal.args["target"]),
            traversal.args["direction"],
            False,  # weighted
            filters,
            sort,
            limit,
            distinct,
        )
    return MaterializedFrame(batch)


# Steps that don't change a NodeFrame's row set, so its id column can be read
# straight from the source without executing anything.
_SEED_PASSTHROUGH = {"from_arrow", "from_polars", "scan_nodes", "nodes"}


def _resolve_seeds(seeds: Any) -> Any:
    """Resolve a hop's seed set to an int64 pyarrow array: an iterable of node ids,
    or a NodeFrame's id column (read from its source when unmodified, else
    collected)."""
    from ._frames import NodeFrame

    if seeds is None:
        raise NotImplementedError(
            "ur.hop(...) needs a seed set: call .from_(ids) with an iterable of node "
            "ids or a NodeFrame."
        )

    import pyarrow as pa

    if isinstance(seeds, NodeFrame):
        plain = all(step.op in _SEED_PASSTHROUGH for step in seeds._plan)
        attr = _resolve_node_attr_table(seeds) if plain else None
        # A plain attribute table yields its ids directly; a derived frame
        # (filtered / computed) must be executed to get them.
        tbl = pa.Table.from_batches([attr]) if attr is not None else seeds.collect().to_arrow()
        name = seeds.id_col if seeds.id_col in tbl.column_names else "id"
        column = tbl.column(name)
        chunks = column.chunks if column.num_chunks else [column.combine_chunks()]
        return pa.concat_arrays(chunks).cast(pa.int64())

    try:
        ids = [int(x) for x in seeds]
    except TypeError as exc:
        raise NotImplementedError(
            "ur.hop(...).from_(seeds) accepts an iterable of node ids or a NodeFrame."
        ) from exc
    return pa.array(ids, type=pa.int64())


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
    index = _require_index(edges)
    batch = _native().run_node_query(
        index, json.dumps(columns), filters, sort, limit, nodes, nodes_id
    )
    return MaterializedFrame(batch)


def _reject_weight(verb: str, payload: dict[str, Any]) -> None:
    """Weighted algorithms are deferred (issue #17). Fail clearly rather than
    silently ignoring a ``weight=`` the caller supplied."""
    if payload.get("weight") is not None:
        raise NotImplementedError(
            f"weighted '{verb}' is not supported yet; only the unweighted form is "
            "wired for v0.1 (weighted algorithms are tracked separately)."
        )


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
    elif verb == "closeness":
        _reject_weight(verb, p)
    elif verb == "betweenness":
        _reject_weight(verb, p)
        column["sample"] = p.get("sample")
    elif verb == "label_propagation":
        column.update(max_iter=p.get("max_iter", 20), seed=p.get("seed"))
    elif verb == "louvain":
        _reject_weight(verb, p)
        column.update(resolution=p.get("resolution", 1.0), seed=p.get("seed"))
    elif verb == "neighbors_agg":
        agg = p["agg"]
        operand = agg.payload.get("operand") if agg.kind == "agg" else None
        if agg.kind != "agg" or operand is None or operand.kind != "col":
            raise NotImplementedError(
                "neighbors().agg() supports ur.col(<name>).<fn>() with fn in "
                "mean/sum/min/max/count/n_unique (mean/sum/min/max need a numeric "
                "attribute column; count/n_unique also accept strings)."
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
        batch = _native().scan_edges_arrow(
            path, scan["src"], scan["dst"], _scan_storage_options(scan)
        )
        return batch.column(0), batch.column(1)

    raise NotImplementedError(
        "collect() needs edges from ur.from_arrow(...), ur.from_polars(...), or "
        "ur.scan_edges(<.parquet|.csv>); this frame has no resolvable source."
    )


def _require_index(edges: EdgeFrame | None) -> Any:
    """The frame's cached native ``GraphIndex`` — the CSR topology built **once**
    and reused by every graph op over this frame (the index-preservation
    contract). Built lazily from ``_require_edges`` on first use and memoized on
    the frame; property-only transforms carry it forward, structural ones drop it
    (see ``EdgeFrame._extend``). Concurrency-safe via a module lock."""
    if edges is None:
        raise NotImplementedError("this expression is not associated with an edge frame.")
    idx = getattr(edges, "_index", None)
    if idx is not None:
        return idx
    with _INDEX_BUILD_LOCK:
        idx = getattr(edges, "_index", None)
        if idx is None:
            src, dst = _require_edges(edges)
            idx = _native().build_index(src, dst)
            edges._index = idx
    return idx


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
