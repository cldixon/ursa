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

# Operators valid at the *top* of a filter predicate — comparisons and boolean
# combinators. (Arithmetic ops may appear deeper, e.g. `(a + b) > 3`, but a bare
# arithmetic or column expression is not itself a boolean predicate.)
_PREDICATE_OPS = {">", "<", ">=", "<=", "==", "!=", "&", "|"}


# --- expression serialization (weights + filter predicates) ----------------
def _expr_to_json(expr: Any, roles: dict[str, str] | None = None) -> dict[str, Any]:
    """Serialize an ``Expr`` tree to the JSON the Rust expr seam parses
    (``ursa_plan::expr::parse_ursa_expr``): ``col`` / ``lit`` / ``binary`` /
    ``unary``. Comparison and boolean ops ride the generic ``binary`` shape.

    ``roles`` maps the abstract role kinds ``src`` / ``dst`` / ``id`` to concrete
    column names in the table the expression will run against. In a **filter**
    context the caller supplies it (the frame is in scope), so a role reference
    lowers to a plain ``col``; a role not present in this context raises. In the
    **weight** context ``roles`` is ``None`` and role/other nodes fall through to a
    ``{"kind": kind}`` stub so the Rust side reports a clear error, exactly as
    before."""
    kind, p = expr.kind, expr.payload
    if kind == "col":
        return {"kind": "col", "name": p["name"]}
    if kind == "lit":
        return {"kind": "lit", "value": p["value"]}
    if kind == "binary":
        return {
            "kind": "binary",
            "op": p["op"],
            "left": _expr_to_json(p["left"], roles),
            "right": _expr_to_json(p["right"], roles),
        }
    if kind == "unary":
        return {"kind": "unary", "op": p["op"], "operand": _expr_to_json(p["operand"], roles)}
    if kind in ("src", "dst", "id"):
        if roles is not None:
            if kind in roles:
                return {"kind": "col", "name": roles[kind]}
            raise NotImplementedError(
                f"ur.{kind}() is not available in this context "
                f"(resolvable roles here: {sorted(roles)})."
            )
        # weight context: let Rust reject role refs with its clear message.
        return {"kind": kind}
    return {"kind": kind}


def _expr_columns(expr: Any, roles: dict[str, str] | None = None) -> set[str]:
    """The (original) column names an expression references — for scan projection
    pushdown. Role refs resolve through ``roles`` to the frame's real columns."""
    kind, p = expr.kind, expr.payload
    if kind == "col":
        return {p["name"]}
    if kind == "binary":
        return _expr_columns(p["left"], roles) | _expr_columns(p["right"], roles)
    if kind == "unary":
        return _expr_columns(p["operand"], roles)
    if kind in ("src", "dst", "id") and roles is not None and kind in roles:
        return {roles[kind]}
    if kind in ("agg", "alias") and "operand" in p:
        return _expr_columns(p["operand"], roles)
    return set()


# --- select (column projection) --------------------------------------------
def _parse_select(columns: Any) -> list[str]:
    """The output column names for a ``select(...)`` step. Accepts bare names or
    ``ur.col(name)`` (Polars-shaped); richer select expressions are future work."""
    names: list[str] = []
    for c in columns:
        if isinstance(c, str):
            names.append(c)
        elif getattr(c, "kind", None) == "col":
            names.append(c.payload["name"])
        else:
            raise NotImplementedError(
                "select() supports column names or ur.col(<name>) for now "
                "(e.g. .select('id', ur.col('pagerank')))."
            )
    return names


def _apply_select(mf: MaterializedFrame, select: list[str] | None) -> MaterializedFrame:
    """Narrow a materialized result to the selected columns (the ``select`` output
    projection). ``select`` never changes rows, only which columns are kept and in
    what order — the Polars-faithful counterpart to additive ``with_columns``."""
    if select is None:
        return mf
    table = mf.to_arrow()
    missing = [c for c in select if c not in table.column_names]
    if missing:
        from . import ColumnNotFoundError

        raise ColumnNotFoundError(f"select() references unknown column(s): {missing}")
    return MaterializedFrame(table.select(select))


def _prepare_weighted(edges: EdgeFrame, weight_columns: set[str]) -> Any | None:
    """The edge attribute table (a **list** of pyarrow RecordBatches) for evaluating
    a weight expression, aligned to the graph's edge-row order.

    The edge table crosses the FFI as a batch list (never concatenated into one
    contiguous batch, #60); ``evaluate_weight`` streams the batches in order, and
    that order matches the topology's ``edge_ids`` because the *same* batch order
    built the index.

    For a ``scan_edges`` frame this does **one** scan projecting ``src``, ``dst``
    and the weight columns together, and builds (memoizes) the frame's topology
    index from those same batches' endpoints. Reading endpoints and weights in the
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
        # The full in-memory edge table shares its row order with the endpoints the
        # index was built from, so its chunks (in order) align with edge_ids.
        return table.select(cols).to_batches()
    scan = getattr(edges, "_scan_spec", None)
    if scan is not None:
        path = scan["path"]
        if not isinstance(path, str):
            raise NotImplementedError(
                "weighted algorithms over a multi-path scan_edges source are not supported yet."
            )
        # One scan → a batch list of [src, dst, *cols]. Build the index from THESE
        # batches' endpoints so weights and edge_ids share a single, self-consistent
        # edge order, overwriting any index memoized from an earlier scan.
        combined = _native().scan_edges_arrow(
            path, scan["src"], scan["dst"], _scan_storage_options(scan), cols
        )
        cell = edges._index_build_cell
        with cell.lock:
            cell.value = _native().build_index(combined)
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
    filters: list[Any] = []  # predicate Exprs, serialized at the FFI boundary
    sort: tuple[str, bool] | None = None
    limit: int | None = None
    select: list[str] | None = None

    for step in frame._plan:
        op = step.op
        # `reverse` is metadata: the reversed edges ride on the edges frame (source
        # swapped), so a graph op over them builds the transpose. `rename` is NOT a
        # passthrough — it must reach the else and raise rather than be dropped.
        if op in ("scan_edges", "scan_nodes", "nodes", "from_arrow", "from_polars", "reverse"):
            continue  # source / metadata steps
        if op == "with_columns":
            # select() defines the output columns; computing more columns after it
            # is ambiguous (which survive?), so require with_columns before select.
            if select is not None:
                raise NotImplementedError(
                    "with_columns() after select() is not supported; compute columns "
                    "first, then select() to narrow the output."
                )
            if graph_exprs is not None:
                raise NotImplementedError(
                    "collect() supports a single with_columns step in a composed pipeline for now."
                )
            graph_exprs = step.args["exprs"]
        elif op == "filter":
            filters.append(step.args["predicate"])
        elif op == "sort":
            sort = _parse_sort(step.args)
        elif op == "head":
            limit = int(step.args["n"])
        elif op == "select":
            if select is not None:
                raise NotImplementedError("collect() supports a single select() step for now.")
            select = _parse_select(step.args["columns"])
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step in a composed pipeline."
            )

    if not graph_exprs:
        # No graph algorithms: a plain source-backed NodeFrame (scan_nodes /
        # from_arrow(id=) / read_nodes). Materialize its attribute table + tail.
        return _collect_plain_nodes(frame, filters, sort, limit, select)

    edges = _single_edges(graph_exprs.values())
    columns = [_algo_column(name, expr) for name, expr in graph_exprs.items()]
    # Column-need analysis for scan projection pushdown (transparent — never changes
    # the output). The computed columns come from the graph node; the file must
    # supply the join id, every column a filter/sort/agg reads, and any *file*
    # column the select keeps. With no select the output is additive (all
    # attributes), so nothing is droppable and the whole file is read.
    computed = set(graph_exprs.keys())
    # Role refs in a filter resolve to the frame's real id column for pushdown.
    referenced: set[str] = set()
    for p in filters:
        referenced |= _expr_columns(p, {"id": frame.id_col})
    if sort is not None:
        referenced.add(sort[0])
    for c in columns:
        if c.get("agg_column") is not None:
            referenced.add(c["agg_column"])
    id_name = frame.id_col
    file_needed: list[str] | None = None
    if select is not None:
        needed = {id_name} | (referenced - computed) | (set(select) - computed)
        file_needed = sorted(needed)
    # If this NodeFrame is a node attribute table, its columns join onto the algo
    # outputs by id (see run_node_query); edges.nodes()-derived frames have none.
    nodes = _resolve_node_attr_table(frame, file_needed)
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
    result = _run_query(edges, columns, filters, sort, limit, nodes, nodes_id, edge_attr)
    return _apply_select(result, select)


def _collect_random_walk(frame: NodeFrame, step: _PlanStep) -> MaterializedFrame:
    """Execute a ``random_walk`` node frame plus an optional
    ``filter``/``sort``/``head``/``distinct`` tail. Its ``(walk_id, step, node)``
    rows are produced by the native walk kernel over the frame's shared index."""
    filters: list[Any] = []
    sort: tuple[str, bool] | None = None
    limit: int | None = None
    distinct = False
    select: list[str] | None = None

    _passthrough = {"random_walk", "nodes", "from_arrow", "from_polars", "scan_nodes"}
    for s in frame._plan:
        op = s.op
        if op in _passthrough:
            continue  # the walk step itself / source / metadata
        elif op == "filter":
            filters.append(s.args["predicate"])
        elif op == "sort":
            sort = _parse_sort(s.args)
        elif op == "head":
            limit = int(s.args["n"])
        elif op == "distinct":
            distinct = True
        elif op == "select":
            if select is not None:
                raise NotImplementedError("collect() supports a single select() step for now.")
            select = _parse_select(s.args["columns"])
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step after a random_walk."
            )

    index = _require_index(step.args["edges"])
    starts = _resolve_seeds(step.args["start"])
    # walk output columns are (walk_id, step, node); no src/dst/id role applies.
    filter_json = [_filter_json(p, {}) for p in filters]
    batch = _native().run_walk_query(
        index,
        starts,
        int(step.args["steps"]),
        int(step.args["walks_per_node"]),
        step.args.get("seed"),
        filter_json,
        sort,
        limit,
        distinct,
    )
    return _apply_select(MaterializedFrame(batch), select)


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


def _resolve_node_attr_table(frame: NodeFrame, columns: list[str] | None = None) -> Any | None:
    """The node attribute table as a chunked ``pyarrow.Table``: an in-memory
    ``from_arrow`` table if present, else a ``scan_nodes`` file source materialized
    through a DataFusion scan. ``edges.nodes()``-derived frames have neither and
    return ``None``.

    ``columns`` is a scan **projection pushdown**: when given, only those columns
    are read from the node file (the join id plus whatever the plan proves it
    needs); ``None`` reads all columns. It is ignored for an in-memory source
    (already resident) — a later ``select`` narrows the output there.

    The scan returns a **list** of RecordBatches (never concatenated, #60); this
    assembles them into one chunked Table. Scan results are memoized on the frame's
    shared attribute cell **keyed by the projected column set**, so repeated
    collects reuse a prior read while a different projection (e.g. a seeds-only
    ``[id]`` read) gets its own cache slot rather than a wrong subset."""
    import pyarrow as pa

    inmem = getattr(frame, "_attr_table", None)
    if inmem is not None:
        # In-memory source is a single RecordBatch; wrap as a one-chunk Table.
        return pa.Table.from_batches([inmem])
    scan = getattr(frame, "_scan_spec", None)
    if scan is not None:
        path = scan["path"]
        if not isinstance(path, str):
            raise NotImplementedError(
                "collect() over a scan_nodes source supports a single string path "
                "(glob included) for now, not a list of paths."
            )
        key = tuple(columns) if columns is not None else None
        cell = frame._attr_build_cell
        cache = cell.value
        if cache is not None and key in cache:
            return cache[key]
        with cell.lock:
            cache = cell.value if cell.value is not None else {}
            if key not in cache:
                batches = _native().scan_nodes_arrow(
                    path, scan["id"], _scan_storage_options(scan), columns or []
                )
                cache[key] = pa.Table.from_batches(batches)
                cell.value = cache
            return cache[key]
    return None


# --- frame-positioned traversals (ur.hop / ur.shortest_path) ----------------
def collect_edge_frame(frame: EdgeFrame) -> MaterializedFrame:
    """Execute a traversal EdgeFrame — ``ur.hop(edges, n).from_(seeds)`` or
    ``ur.shortest_path(edges, s, t)`` — plus an optional
    ``filter``/``sort``/``head``/``distinct`` tail."""
    traversal = None
    filters: list[Any] = []
    sort: tuple[str, bool] | None = None
    limit: int | None = None
    distinct = False
    select: list[str] | None = None

    for step in frame._plan:
        op = step.op
        if op in ("hop", "shortest_path"):
            traversal = step
        elif op in ("scan_edges", "from_arrow", "from_polars", "nodes"):
            continue  # source / metadata steps
        elif op == "reverse":
            # Metadata on a plain frame (the swapped source rides on the frame).
            # After a traversal its semantics are undesigned -> reject rather than
            # silently ignore. `rename` is not a passthrough: it falls to the else
            # and raises instead of being dropped.
            if traversal is not None:
                raise NotImplementedError(
                    "reverse() after a traversal (hop/shortest_path) is not supported; "
                    "reverse the edges before the traversal."
                )
            continue
        elif op == "filter":
            filters.append(step.args["predicate"])
        elif op == "sort":
            sort = _parse_sort(step.args)
        elif op == "head":
            limit = int(step.args["n"])
        elif op == "distinct":
            distinct = True
        elif op == "select":
            if select is not None:
                raise NotImplementedError("collect() supports a single select() step for now.")
            select = _parse_select(step.args["columns"])
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step on an edge frame."
            )

    if traversal is None:
        # No traversal: a plain edge frame (scan_edges / from_arrow / read_edges,
        # or a reversed frame). Materialize its edge rows + filter/sort/head tail.
        if distinct:
            raise NotImplementedError("collect() distinct on a plain edge frame is not wired yet.")
        return _collect_plain_edges(frame, filters, sort, limit, select)

    # A traversal result carries reserved `src`/`dst` columns (+ a path cost for
    # weighted shortest_path); ur.src()/dst() in a filter resolve to them.
    filter_json = [_filter_json(p, {"src": "src", "dst": "dst"}) for p in filters]
    if traversal.op == "hop":
        index = _require_index(traversal.args["edges"])
        seeds = _resolve_seeds(traversal.args["seeds"])
        batch = _native().run_hop_query(
            index,
            seeds,
            int(traversal.args["n"]),
            traversal.args["direction"],
            filter_json,
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
            filter_json,
            sort,
            limit,
            distinct,
        )
    return _apply_select(MaterializedFrame(batch), select)


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
        # As seeds, only the id column is needed — project the scan to just [id]
        # (a real, non-lossy projection-pushdown win; a 50-column node file used
        # only for seeds reads one column).
        attr = _resolve_node_attr_table(seeds, [seeds.id_col]) if plain else None
        # A plain attribute table yields its ids directly; a derived frame
        # (filtered / computed) must be executed to get them.
        if attr is not None:
            tbl = attr  # a (chunked) pyarrow.Table
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
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    nodes: Any | None = None,
    nodes_id: str | None = None,
    edge_attr: Any | None = None,
) -> MaterializedFrame:
    index = _require_index(edges)
    # The node attribute table crosses the FFI as a batch list (not one batch).
    nodes_batches = nodes.to_batches() if nodes is not None else None
    # A node-query result carries the reserved `id` column plus the computed/joined
    # columns; `ur.id()` in a filter resolves to it. (src/dst have no role here.)
    filter_json = [_filter_json(p, {"id": "id"}) for p in filters]
    batches = _native().run_node_query(
        index, json.dumps(columns), filter_json, sort, limit, nodes_batches, nodes_id, edge_attr
    )
    return MaterializedFrame(batches)


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
        # weak = undirected union-find; strong = Tarjan SCC. Any other mode is a
        # typo — raise rather than silently computing weak.
        mode = p.get("mode", "weak")
        if mode not in ("weak", "strong"):
            raise NotImplementedError(
                f"connected_components(mode={mode!r}) is not supported; "
                "use mode='weak' or mode='strong'."
            )
        column["mode"] = mode
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


def _require_edges(edges: EdgeFrame | None) -> list[Any]:
    """Resolve an EdgeFrame to a **batch list** of ``[src, dst]`` RecordBatches
    (endpoints canonicalized to int64 or string) ready for ``build_index``, or raise
    clearly. The list is fed to the CSR build as a stream — never concatenated into
    one contiguous batch first (#60)."""
    if edges is None:
        raise NotImplementedError("this expression is not associated with an edge frame.")

    arrays = getattr(edges, "_edge_arrays", None)
    if arrays is not None:
        import pyarrow as pa

        src, dst = arrays
        return [pa.RecordBatch.from_arrays([src, dst], ["src", "dst"])]

    scan = getattr(edges, "_scan_spec", None)
    if scan is not None:
        path = scan["path"]
        if not isinstance(path, str):
            raise NotImplementedError(
                "collect() over a scan_edges source supports a single string path "
                "(glob included) for now, not a list of paths."
            )
        # The scan returns a list of [src, dst] batches; build_index reads columns
        # 0/1 of each, so it is passed straight through.
        return _native().scan_edges_arrow(
            path, scan["src"], scan["dst"], _scan_storage_options(scan)
        )

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
            edge_batches = _require_edges(edges)
            idx = _native().build_index(edge_batches)
            cell.value = idx
    return idx


def _require_predicate(predicate: Any) -> None:
    """Reject a ``filter()`` argument that isn't a boolean predicate at its top node
    — a bare ``col`` or an arithmetic expression is not a filter. Enforced
    identically on both the graph-op (Rust) and plain-frame (pyarrow) paths, so the
    same input is accepted or rejected the same way regardless of the source."""
    kind = predicate.kind
    top_binary_ok = kind == "binary" and predicate.payload["op"] in _PREDICATE_OPS
    top_unary_ok = kind == "unary" and predicate.payload["op"] == "~"
    if not (top_binary_ok or top_unary_ok):
        raise NotImplementedError(
            "filter() expects a boolean predicate — a comparison (>, >=, <, <=, ==, !=) "
            "or a boolean combination (&, |, ~), e.g. .filter(ur.col('deg') > 2) or "
            ".filter((ur.col('a') > 1) & (ur.col('b') == 'x'))."
        )


def _filter_json(predicate: Any, roles: dict[str, str]) -> str:
    """Serialize a filter ``predicate`` (an ``Expr``) to the JSON string the Rust
    expr seam lowers to a DataFusion filter. The predicate may use the full algebra
    — comparisons, boolean ``& | ~``, arithmetic, ``col <op> col``, string/bool
    equality, and role refs (``ur.src()``/``dst()``/``id()``, resolved via
    ``roles``). The top node must be a boolean predicate (see ``_require_predicate``)."""
    _require_predicate(predicate)
    return json.dumps(_expr_to_json(predicate, roles))


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
# pyarrow.compute kernel names by op (looked up by name so the type checker —
# which lacks pyarrow.compute stubs — doesn't flag each one).
_PC_BINARY = {
    "+": "add",
    "-": "subtract",
    "*": "multiply",
    "/": "divide",
    ">": "greater",
    ">=": "greater_equal",
    "<": "less",
    "<=": "less_equal",
    "==": "equal",
    "!=": "not_equal",
    "&": "and_kleene",
    "|": "or_kleene",
}
_PC_UNARY = {"~": "invert"}


def _eval_pyarrow(expr: Any, table: Any, roles: dict[str, str]) -> Any:
    """Evaluate an ``Expr`` predicate tree over a pyarrow ``table``, mirroring the
    Rust ``expr::lower`` semantics for the plain-frame path (which never touches the
    engine). Returns a pyarrow Array/scalar; the top-level result is a boolean mask."""
    import pyarrow as pa
    import pyarrow.compute as pc

    kind, p = expr.kind, expr.payload
    if kind == "col":
        name = p["name"]
        if name not in table.column_names:
            raise ValueError(f"filter references unknown column '{name}'.")
        return table.column(name)
    if kind == "lit":
        return pa.scalar(p["value"])
    if kind in ("src", "dst", "id"):
        if kind not in roles:
            raise NotImplementedError(
                f"ur.{kind}() is not available in this context "
                f"(resolvable roles here: {sorted(roles)})."
            )
        name = roles[kind]
        if name not in table.column_names:
            raise ValueError(f"filter references unknown column '{name}'.")
        return table.column(name)
    if kind == "binary":
        op = p["op"]
        if op not in _PC_BINARY:
            raise NotImplementedError(f"filter operator {op!r} is not supported.")
        left = _eval_pyarrow(p["left"], table, roles)
        right = _eval_pyarrow(p["right"], table, roles)
        return getattr(pc, _PC_BINARY[op])(left, right)
    if kind == "unary":
        if p["op"] not in _PC_UNARY:
            raise NotImplementedError(f"unary filter operator {p['op']!r} is not supported.")
        return getattr(pc, _PC_UNARY[p["op"]])(_eval_pyarrow(p["operand"], table, roles))
    raise NotImplementedError(
        f"filter expression node {kind!r} is not supported (aggregations belong in group_by)."
    )


def _apply_pyarrow_tail(
    table: Any,
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    roles: dict[str, str],
) -> Any:
    for predicate in filters:
        _require_predicate(predicate)  # same top-of-predicate check as the graph path
        table = table.filter(_eval_pyarrow(predicate, table, roles))
    if sort is not None:
        by, descending = sort
        if by not in table.column_names:
            raise ValueError(f"sort references unknown column '{by}'.")
        table = table.sort_by([(by, "descending" if descending else "ascending")])
    if limit is not None:
        table = table.slice(0, limit)
    return table


def _collect_plain_nodes(
    frame: NodeFrame,
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    select: list[str] | None = None,
) -> MaterializedFrame:
    # On a plain node table, ur.id() resolves to the frame's real id column.
    roles = {"id": frame.id_col}
    # With no computed columns, the scan need only read the id, whatever the
    # filter/sort touch, and (if a select narrows the output) the selected columns.
    file_needed: list[str] | None = None
    if select is not None:
        referenced: set[str] = set()
        for p in filters:
            referenced |= _expr_columns(p, roles)
        if sort is not None:
            referenced.add(sort[0])
        file_needed = sorted({frame.id_col} | referenced | set(select))
    attr = _resolve_node_attr_table(frame, file_needed)
    if attr is None:
        raise NotImplementedError(
            "collect() on this NodeFrame needs a source (scan_nodes / from_arrow(id=...)) "
            "or a with_columns of graph algorithms; a bare edges.nodes() has no "
            "materializable node set yet."
        )
    # `attr` is a (chunked) pyarrow.Table.
    table = _apply_pyarrow_tail(attr, filters, sort, limit, roles)
    return _apply_select(MaterializedFrame(table), select)


def _collect_plain_edges(
    frame: EdgeFrame,
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    select: list[str] | None = None,
) -> MaterializedFrame:
    import pyarrow as pa

    # On a plain edge table, ur.src()/dst() resolve to the frame's real endpoint cols.
    roles = {"src": frame.src_col, "dst": frame.dst_col}
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
            batches = _native().scan_edges_arrow(
                path, scan["src"], scan["dst"], _scan_storage_options(scan), []
            )
            table = pa.Table.from_batches(batches).rename_columns([scan["src"], scan["dst"]])
        else:
            # No source: a filtered/derived (or traversal-result) edge frame.
            _require_edges(frame)  # raises the precise, plan-aware message
    table = _apply_pyarrow_tail(table, filters, sort, limit, roles)
    return _apply_select(MaterializedFrame(table), select)


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
