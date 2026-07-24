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
    from ._frames import EdgeFrame, NodeFrame, _PlanStep

# The lazy index build is guarded per-frame by the lock on each frame's shared
# `_BuildCell` (see `_frames._BuildCell`), so first-collects of *unrelated* frames
# from different threads build concurrently instead of serializing on one global
# lock. collect() runs GIL-free, so the memo assignment itself still needs the
# cell's lock.

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


# --- weight expressions (over edge columns) --------------------------------
def _expr_to_json(expr: Any) -> dict[str, Any]:
    """Serialize an ``Expr`` tree to the JSON the Rust weight seam parses
    (``ursa_plan::expr::parse_ursa_expr``): ``col`` / ``lit`` / ``binary``. Other
    node kinds are emitted by kind so the Rust side reports a clear error."""
    kind, p = expr.kind, expr.payload
    if kind == "col":
        return {"kind": "col", "name": p["name"]}
    if kind == "lit":
        return {"kind": "lit", "value": p["value"]}
    if kind == "binary":
        return {
            "kind": "binary",
            "op": p["op"],
            "left": _expr_to_json(p["left"]),
            "right": _expr_to_json(p["right"]),
        }
    return {"kind": kind}


def _expr_columns(expr: Any) -> set[str]:
    """The edge column names a weight expression references."""
    kind, p = expr.kind, expr.payload
    if kind == "col":
        return {p["name"]}
    if kind == "binary":
        return _expr_columns(p["left"]) | _expr_columns(p["right"])
    return set()


def _prepare_weighted(edges: EdgeFrame, weight_columns: set[str]) -> Any | None:
    """The edge attribute batch (a pyarrow RecordBatch) for evaluating a weight
    expression, aligned to the graph's edge-row order.

    For a ``scan_edges`` frame this does **one** scan projecting ``src``, ``dst``
    and the weight columns together, and builds (memoizes) the frame's topology
    index from that same batch's endpoints. Reading endpoints and weights in the
    same scan is what makes ``weights[edge_ids[k]]`` correct: a second, independent
    scan could order partitions differently (globs/multi-partition listing order is
    not a DataFusion contract), silently misaligning every weighted result (#37).
    In-memory frames already share one retained table, so they just subset it."""
    if not weight_columns:
        return None
    cols = sorted(weight_columns)
    table = getattr(edges, "_edge_attr_table", None)
    if table is not None:
        missing = [c for c in cols if c not in table.column_names]
        if missing:
            from . import ColumnNotFoundError

            raise ColumnNotFoundError(
                f"weight expression references unknown edge column(s): {missing}"
            )
        batches = table.select(cols).combine_chunks().to_batches()
        return batches[0] if batches else None
    scan = getattr(edges, "_scan_spec", None)
    if scan is not None:
        path = scan["path"]
        if not isinstance(path, str):
            raise NotImplementedError(
                "weighted algorithms over a multi-path scan_edges source are not supported yet."
            )
        # One scan → [src, dst, *cols]. Build the index from THIS batch's endpoints
        # so weights and edge_ids share a single, self-consistent edge order,
        # overwriting any index memoized from an earlier endpoints-only scan.
        combined = _native().scan_edges_arrow(
            path, scan["src"], scan["dst"], _scan_storage_options(scan), cols
        )
        cell = edges._index_build_cell
        with cell.lock:
            cell.value = _native().build_index(combined.column(0), combined.column(1))
        return combined
    raise NotImplementedError(
        "a weighted algorithm needs an in-memory or scan_edges source so the weight "
        "columns can be read."
    )


# --- composed with_columns pipeline ----------------------------------------
# (Standalone `ur.pagerank(edges).collect()` promotes to a NodeFrame via
# GraphExpr and also lands here — see ursa._graph.GraphExpr.)
def collect_node_frame(frame: NodeFrame) -> MaterializedFrame:
    """Execute a composed ``edges.nodes().with_columns(...).filter/sort/head``."""
    # describe() is a whole-graph summary, computed eagerly off the topology and
    # wrapped as a one-row frame (its own branch — not a per-node algorithm).
    describe_step = next((s for s in frame._plan if s.op == "describe"), None)
    if describe_step is not None:
        if any(s.op != "describe" for s in frame._plan):
            raise NotImplementedError(
                "describe() output does not support a filter/sort/head tail yet; "
                "call describe() as the final step of the pipeline."
            )
        index = _require_index(describe_step.args["edges"])
        batch = _native().graph_describe(index, bool(describe_step.args.get("full", False)))
        return MaterializedFrame(batch)

    walk_step = next((s for s in frame._plan if s.op == "random_walk"), None)
    if walk_step is not None:
        return _collect_random_walk(frame, walk_step)

    graph_exprs: dict[str, Expr] | None = None
    filters: list[tuple[str, str, float]] = []
    sort: tuple[str, bool] | None = None
    limit: int | None = None

    for step in frame._plan:
        op = step.op
        # `reverse` is metadata: the reversed edges ride on the edges frame (source
        # swapped), so a graph op over them builds the transpose. `rename` is NOT a
        # passthrough — it must reach the else and raise rather than be dropped.
        if op in ("scan_edges", "scan_nodes", "nodes", "from_arrow", "from_polars", "reverse"):
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
        # No graph algorithms: a plain source-backed NodeFrame (scan_nodes /
        # from_arrow(id=) / read_nodes). Materialize its attribute table + tail.
        return _collect_plain_nodes(frame, filters, sort, limit)

    edges = _single_edges(graph_exprs.values())
    columns = [_algo_column(name, expr) for name, expr in graph_exprs.items()]
    # If this NodeFrame is a node attribute table, its columns join onto the algo
    # outputs by id (see run_node_query); edges.nodes()-derived frames have none.
    nodes = _resolve_node_attr_table(frame)
    nodes_id = frame.id_col if nodes is not None else None
    # A weighted algorithm needs the edge columns its weight expression references.
    weight_cols: set[str] = set()
    for expr in graph_exprs.values():
        w = expr.payload.get("weight") if expr.kind == "graph" else None
        if w is not None:
            weight_cols |= _expr_columns(w)
    # _prepare_weighted (for a scan source) also (re)builds the frame's index from
    # the same scan, so _run_query's _require_index reuses it — endpoints and
    # weights share one edge order.
    edge_attr = _prepare_weighted(edges, weight_cols) if weight_cols else None
    return _run_query(edges, columns, filters, sort, limit, nodes, nodes_id, edge_attr)


def _collect_random_walk(frame: NodeFrame, step: _PlanStep) -> MaterializedFrame:
    """Execute a ``random_walk`` node frame plus an optional
    ``filter``/``sort``/``head``/``distinct`` tail. Its ``(walk_id, step, node)``
    rows are produced by the native walk kernel over the frame's shared index."""
    filters: list[tuple[str, str, float]] = []
    sort: tuple[str, bool] | None = None
    limit: int | None = None
    distinct = False

    _passthrough = {"random_walk", "nodes", "from_arrow", "from_polars", "scan_nodes"}
    for s in frame._plan:
        op = s.op
        if op in _passthrough:
            continue  # the walk step itself / source / metadata
        elif op == "filter":
            filters.append(_parse_filter(s.args["predicate"]))
        elif op == "sort":
            sort = _parse_sort(s.args)
        elif op == "head":
            limit = int(s.args["n"])
        elif op == "distinct":
            distinct = True
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step after a random_walk."
            )

    index = _require_index(step.args["edges"])
    starts = _resolve_seeds(step.args["start"])
    batch = _native().run_walk_query(
        index,
        starts,
        int(step.args["steps"]),
        int(step.args["walks_per_node"]),
        step.args.get("seed"),
        filters,
        sort,
        limit,
        distinct,
    )
    return MaterializedFrame(batch)


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
    scan. ``edges.nodes()``-derived frames have neither and return ``None``.

    A scan-backed source is read from disk **once** and memoized on the frame's
    shared attribute cell, so repeated collects (and ``_resolve_seeds``) over one
    node file don't re-scan it each time."""
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
        cell = frame._attr_build_cell
        batch = cell.value
        if batch is not None:
            return batch
        with cell.lock:
            batch = cell.value
            if batch is None:
                batch = _native().scan_nodes_arrow(path, scan["id"], _scan_storage_options(scan))
                cell.value = batch
        return batch
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
        elif op in ("scan_edges", "from_arrow", "from_polars", "nodes"):
            continue  # source / metadata steps
        elif op == "reverse":
            # Metadata on a plain frame (the swapped source rides on the frame).
            # After a traversal its semantics are undesigned -> reject rather than
            # silently ignore. `select`/`rename` are not passthroughs: they fall to
            # the else and raise instead of being dropped.
            if traversal is not None:
                raise NotImplementedError(
                    "reverse() after a traversal (hop/shortest_path) is not supported; "
                    "reverse the edges before the traversal."
                )
            continue
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
                f"collect() does not yet support the '{op}' step on an edge frame."
            )

    if traversal is None:
        # No traversal: a plain edge frame (scan_edges / from_arrow / read_edges,
        # or a reversed frame). Materialize its edge rows + filter/sort/head tail.
        if distinct:
            raise NotImplementedError("collect() distinct on a plain edge frame is not wired yet.")
        return _collect_plain_edges(frame, filters, sort, limit)

    if traversal.op == "hop":
        index = _require_index(traversal.args["edges"])
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
        # A weight= expression (over edge columns) selects weighted Dijkstra: it's
        # serialized and evaluated in Rust against the edge attribute batch.
        weight = traversal.args.get("weight")
        weight_json = None
        edge_attr = None
        if weight is not None:
            weight_json = json.dumps(_expr_to_json(weight))
            # Prepare weights first: for a scan source this rebuilds the index from
            # the same scan, so _require_index (below) reuses the aligned index.
            edge_attr = _prepare_weighted(traversal.args["edges"], _expr_columns(weight))
        index = _require_index(traversal.args["edges"])
        # source/target cross as 1-element user-id arrays (int64 or string),
        # resolved to dense indices in Rust — the same path as hop seeds.
        batch = _native().run_path_query(
            index,
            _resolve_seeds([traversal.args["source"]]),
            _resolve_seeds([traversal.args["target"]]),
            traversal.args["direction"],
            weight_json,
            edge_attr,
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
    """Resolve a seed set to a pyarrow array of user ids (int64 or string, matching
    the graph): an iterable of node ids, or a NodeFrame's id column (read from its
    source when unmodified, else collected)."""
    from ._frames import NodeFrame
    from ._io import _canonical_id_array

    if seeds is None:
        raise NotImplementedError(
            "a seed set is required: pass an iterable of node ids or a NodeFrame "
            "(ur.hop(...).from_(ids); ur.random_walk(..., start=ids))."
        )

    import pyarrow as pa

    if isinstance(seeds, NodeFrame):
        plain = all(step.op in _SEED_PASSTHROUGH for step in seeds._plan)
        attr = _resolve_node_attr_table(seeds) if plain else None
        # A plain attribute table yields its ids directly; a derived frame
        # (filtered / computed) must be executed to get them.
        if attr is not None:
            tbl = pa.Table.from_batches([attr])
        else:
            try:
                tbl = seeds.collect().to_arrow()
            except NotImplementedError as exc:
                raise NotImplementedError(
                    "the seed NodeFrame could not be materialized. Seeding a traversal / "
                    "walk from a derived node frame (filtered, or with computed columns) "
                    "needs that frame to be collectable; pass an explicit id iterable, a "
                    "plain scan_nodes/from_arrow(id=...) frame, or precompute the ids. "
                    f"(underlying: {exc})"
                ) from exc
        name = seeds.id_col if seeds.id_col in tbl.column_names else "id"
        column = tbl.column(name)
        chunks = column.chunks if column.num_chunks else [column.combine_chunks()]
        return _canonical_id_array(pa.concat_arrays(chunks))

    try:
        vals = list(seeds)
    except TypeError as exc:
        raise NotImplementedError(
            "seed ids must be an iterable of node ids or a NodeFrame."
        ) from exc
    # Infer the id type from the values: all strings -> string, otherwise int64.
    # (bool is an int subclass, so exclude it from the integer path.)
    if vals and all(isinstance(v, str) for v in vals):
        return pa.array(vals, type=pa.string())
    try:
        return pa.array([int(v) for v in vals], type=pa.int64())
    except (TypeError, ValueError) as exc:
        raise NotImplementedError("seed ids must be all integers or all strings.") from exc


# --- the one execution entry ------------------------------------------------
def _run_query(
    edges: EdgeFrame | None,
    columns: list[dict[str, Any]],
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None,
    limit: int | None,
    nodes: Any | None = None,
    nodes_id: str | None = None,
    edge_attr: Any | None = None,
) -> MaterializedFrame:
    index = _require_index(edges)
    batch = _native().run_node_query(
        index, json.dumps(columns), filters, sort, limit, nodes, nodes_id, edge_attr
    )
    return MaterializedFrame(batch)


def _add_weight(column: dict[str, Any], payload: dict[str, Any]) -> None:
    """Attach a serialized weight expression to a column spec, if one was given.
    Supported on pagerank / closeness / betweenness / louvain."""
    weight = payload.get("weight")
    if weight is not None:
        column["weight"] = _expr_to_json(weight)


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
    if verb == "connected_components":
        # Only weak components are wired; strong is a later release (SPEC). The
        # Rust side hardcodes weak, so a non-weak mode must raise here rather than
        # be silently computed as weak.
        mode = p.get("mode", "weak")
        if mode != "weak":
            raise NotImplementedError(
                f"connected_components(mode={mode!r}) is not supported yet; "
                "only mode='weak' is wired."
            )
    if verb == "pagerank":
        column.update(
            damping=p.get("damping", 0.85),
            max_iter=p.get("max_iter", 30),
            tol=p.get("tol", 1e-6),
        )
        _add_weight(column, p)
    elif verb == "degree":
        column["direction"] = p.get("direction", "out")
    elif verb == "closeness":
        _add_weight(column, p)
    elif verb == "betweenness":
        column["sample"] = p.get("sample")
        column["seed"] = p.get("seed")
        _add_weight(column, p)
    elif verb == "label_propagation":
        column.update(max_iter=p.get("max_iter", 20), seed=p.get("seed"))
    elif verb == "louvain":
        column.update(resolution=p.get("resolution", 1.0), seed=p.get("seed"))
        _add_weight(column, p)
    elif verb == "neighbors_agg":
        # from_= (resolve the aggregation against a different node frame) isn't
        # wired: it would otherwise be recorded and silently ignored.
        if p.get("from_") is not None:
            raise NotImplementedError(
                "neighbors(from_=...) is not supported yet; the aggregation resolves "
                "against the frame it runs in."
            )
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
    # A weight= on a verb that doesn't consume it (e.g. label_propagation) would be
    # silently ignored otherwise — fail clearly instead.
    if p.get("weight") is not None and "weight" not in column:
        raise NotImplementedError(f"weight= is not supported for '{verb}'.")
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

    # No source: give a message matched to *why* it's missing, not a generic one.
    ops = {step.op for step in getattr(edges, "_plan", ())}
    if ops & {"hop", "shortest_path"}:
        raise NotImplementedError(
            "chaining a graph op off a traversal result (hop/shortest_path) isn't "
            "wired yet — a traversal result is a set of reached edges with no rebuildable "
            "topology source. Collect it and re-ingest via ur.from_arrow(...) to run "
            "further ops on it. (v0.2: child-plan seeding.)"
        )
    if ops & {"filter", "distinct", "sample", "join", "group_by_agg"}:
        raise NotImplementedError(
            "graph ops on a filtered/derived edge frame aren't wired yet — filtering "
            "edges before a graph op needs the DataFusion edge pipeline. Run the op on "
            "the source frame, or collect and re-ingest the filtered edges."
        )
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
    cell = edges._index_build_cell
    idx = cell.value
    if idx is not None:
        return idx
    with cell.lock:
        idx = cell.value
        if idx is None:
            src, dst = _require_edges(edges)
            idx = _native().build_index(src, dst)
            cell.value = idx
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


# --- plain-source collection (no algorithm/traversal) ----------------------
# Materialize a frame's own rows and apply the filter/sort/head tail in pyarrow.
# Used by read_edges/read_nodes and scan_*/from_* frames collected without a
# graph op (e.g. scan_edges(...).to_polars()).
def _apply_pyarrow_tail(
    table: Any,
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None,
    limit: int | None,
) -> Any:
    import pyarrow.compute as pc

    # pyarrow.compute's comparison kernels, by op (looked up by name so the type
    # checker — which lacks pyarrow.compute stubs — doesn't flag each one).
    cmp = {
        ">": "greater",
        ">=": "greater_equal",
        "<": "less",
        "<=": "less_equal",
        "==": "equal",
        "!=": "not_equal",
    }
    for column, op, value in filters:
        if column not in table.column_names:
            raise ValueError(f"filter references unknown column '{column}'.")
        table = table.filter(getattr(pc, cmp[op])(table.column(column), value))
    if sort is not None:
        by, descending = sort
        if by not in table.column_names:
            raise ValueError(f"sort references unknown column '{by}'.")
        table = table.sort_by([(by, "descending" if descending else "ascending")])
    if limit is not None:
        table = table.slice(0, limit)
    return table


def _table_to_batch(table: Any) -> Any:
    """A single pyarrow RecordBatch for a (possibly multi-chunk or empty) Table."""
    import pyarrow as pa

    table = table.combine_chunks()
    batches = table.to_batches()
    if batches:
        return batches[0]
    # zero-row result: an empty batch that still carries the schema
    return pa.RecordBatch.from_arrays(
        [pa.array([], type=field.type) for field in table.schema], schema=table.schema
    )


def _collect_plain_nodes(
    frame: NodeFrame,
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None,
    limit: int | None,
) -> MaterializedFrame:
    import pyarrow as pa

    attr = _resolve_node_attr_table(frame)
    if attr is None:
        raise NotImplementedError(
            "collect() on this NodeFrame needs a source (scan_nodes / from_arrow(id=...)) "
            "or a with_columns of graph algorithms; a bare edges.nodes() has no "
            "materializable node set yet."
        )
    table = _apply_pyarrow_tail(pa.Table.from_batches([attr]), filters, sort, limit)
    return MaterializedFrame(_table_to_batch(table))


def _collect_plain_edges(
    frame: EdgeFrame,
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None,
    limit: int | None,
) -> MaterializedFrame:
    import pyarrow as pa

    table = getattr(frame, "_edge_attr_table", None)  # full in-memory edge table
    if table is None:
        scan = getattr(frame, "_scan_spec", None)
        if scan is not None:
            path = scan["path"]
            if not isinstance(path, str):
                raise NotImplementedError(
                    "collect() over a scan_edges source supports a single string path "
                    "(glob included) for now, not a list of paths."
                )
            batch = _native().scan_edges_arrow(
                path, scan["src"], scan["dst"], _scan_storage_options(scan), []
            )
            table = pa.Table.from_batches([batch]).rename_columns([scan["src"], scan["dst"]])
        else:
            # No source: a filtered/derived (or traversal-result) edge frame.
            _require_edges(frame)  # raises the precise, plan-aware message
    table = _apply_pyarrow_tail(table, filters, sort, limit)
    return MaterializedFrame(_table_to_batch(table))


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
