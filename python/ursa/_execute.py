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
    if kind == "agg":
        return {"kind": "agg", "fn": p["fn"], "operand": _expr_to_json(p["operand"], roles)}
    if kind == "alias":
        return {"kind": "alias", "name": p["name"], "operand": _expr_to_json(p["operand"], roles)}
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
    # A weighted algorithm on a derived edge frame must be rejected here too: this
    # function builds the index directly (scan branch) and would otherwise populate
    # the cell with a wrong topology before `_require_index`'s guard could run.
    _reject_derived_edge_graph_op(edges)
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
    distinct = False
    sample: tuple[int, int | None] | None = None
    rename: dict[str, str] = {}
    group_by: tuple[list[str], list[str], list[dict[str, str]]] | None = None
    saw_group_by = False
    join: tuple[Any, list[str], str] | None = None

    for step in frame._plan:
        op = step.op
        # `reverse` is metadata: the reversed edges ride on the edges frame (source
        # swapped), so a graph op over them builds the transpose.
        if op in ("scan_edges", "scan_nodes", "nodes", "from_arrow", "from_polars", "reverse"):
            continue  # source / metadata steps
        if op == "join":
            if join is not None:
                raise NotImplementedError("collect() supports a single join() for now.")
            join = _parse_join(step)
            continue
        if op == "group_by_agg":
            if group_by is not None:
                raise NotImplementedError("collect() supports a single group_by().agg() for now.")
            _reject_pre_group_tail(sort, limit, rename)
            group_by = _parse_group_agg(step)
            saw_group_by = True
            continue
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
            if saw_group_by:
                raise NotImplementedError(
                    "filter() after group_by().agg() (SQL HAVING) is not supported yet; "
                    "filter before the group_by."
                )
            filters.append(step.args["predicate"])
        elif op == "sort":
            sort = _parse_sort(step.args)
        elif op == "head":
            limit = int(step.args["n"])
        elif op == "distinct":
            distinct = True
        elif op == "sample":
            sample = (int(step.args["n"]), step.args.get("seed"))
        elif op == "rename":
            rename.update(step.args["mapping"])
        elif op == "select":
            if select is not None:
                raise NotImplementedError("collect() supports a single select() step for now.")
            select = _parse_select(step.args["columns"])
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step in a composed pipeline."
            )

    if group_by is not None and (distinct or sample is not None or select is not None):
        raise NotImplementedError(
            "group_by().agg() does not compose with distinct/sample/select yet; "
            "sort/head/rename on the grouped result are supported."
        )

    if join is not None:
        if group_by is not None:
            raise NotImplementedError(
                "join() does not compose with group_by().agg() yet; do them in "
                "separate collect() steps."
            )
        # The left table is the raw base (empty tail — the tail applies post-join):
        # a graph-derived NodeFrame runs its algorithms first, a plain one uses its
        # attribute table. ur.id() in a post-join filter resolves to the real id.
        if graph_exprs:
            edges = _single_edges(graph_exprs.values())
            columns = [_algo_column(name, expr) for name, expr in graph_exprs.items()]
            nodes = _resolve_node_attr_table(frame, None)
            nodes_id = frame.id_col if nodes is not None else None
            weight_cols: set[str] = set()
            for expr in graph_exprs.values():
                w = expr.payload.get("weight") if expr.kind == "graph" else None
                if w is not None:
                    weight_cols |= _expr_columns(w)
            edge_attr = _prepare_weighted(edges, weight_cols) if weight_cols else None
            left_table = _run_query(
                edges, columns, [], None, None, nodes, nodes_id, edge_attr
            ).to_arrow()
        else:
            attr = _resolve_node_attr_table(frame, None)
            if attr is None:
                raise NotImplementedError(
                    "join() on this NodeFrame needs a source (scan_nodes / "
                    "from_arrow(id=...)) or a with_columns of graph algorithms."
                )
            left_table = attr
        return _run_join(
            left_table,
            join,
            filters,
            sort,
            limit,
            select,
            sample,
            rename,
            distinct,
            {"id": frame.id_col},
        )

    if not graph_exprs:
        # No graph algorithms: a plain source-backed NodeFrame (scan_nodes /
        # from_arrow(id=) / read_nodes). Materialize its attribute table + tail.
        return _collect_plain_nodes(
            frame, filters, sort, limit, select, sample, rename, distinct, group_by
        )

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
    # A rename reads its source (old) columns; keep any that are file columns.
    referenced |= set(rename)
    id_name = frame.id_col
    file_needed: list[str] | None = None
    if select is not None:
        # A rename *target* is a new output label, neither a file nor a computed
        # column, so exclude it from the file projection (else the scan errors on a
        # column the file doesn't have).
        selected_file = set(select) - computed - set(rename.values())
        needed = {id_name} | (referenced - computed) | selected_file
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
    result = _run_query(
        edges,
        columns,
        filters,
        sort,
        limit,
        nodes,
        nodes_id,
        edge_attr,
        distinct=distinct,
        sample=sample,
        rename=rename,
        group_by=group_by,
    )
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
    sample: tuple[int, int | None] | None = None
    rename: dict[str, str] = {}

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
        elif op == "sample":
            sample = (int(s.args["n"]), s.args.get("seed"))
        elif op == "rename":
            rename.update(s.args["mapping"])
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
        sample,
        list(rename.items()),
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
    sample: tuple[int, int | None] | None = None
    rename: dict[str, str] = {}
    group_by: tuple[list[str], list[str], list[dict[str, str]]] | None = None
    saw_group_by = False
    join: tuple[Any, list[str], str] | None = None

    for step in frame._plan:
        op = step.op
        if op in ("hop", "shortest_path"):
            traversal = step
        elif op in ("scan_edges", "from_arrow", "from_polars", "nodes"):
            continue  # source / metadata steps
        elif op == "join":
            if join is not None:
                raise NotImplementedError("collect() supports a single join() for now.")
            join = _parse_join(step)
        elif op == "group_by_agg":
            if group_by is not None:
                raise NotImplementedError("collect() supports a single group_by().agg() for now.")
            _reject_pre_group_tail(sort, limit, rename)
            group_by = _parse_group_agg(step)
            saw_group_by = True
        elif op == "reverse":
            # Metadata on a plain frame (the swapped source rides on the frame).
            # After a traversal its semantics are undesigned -> reject rather than
            # silently ignore.
            if traversal is not None:
                raise NotImplementedError(
                    "reverse() after a traversal (hop/shortest_path) is not supported; "
                    "reverse the edges before the traversal."
                )
            continue
        elif op == "filter":
            if saw_group_by:
                raise NotImplementedError(
                    "filter() after group_by().agg() (SQL HAVING) is not supported yet; "
                    "filter before the group_by."
                )
            filters.append(step.args["predicate"])
        elif op == "sort":
            sort = _parse_sort(step.args)
        elif op == "head":
            limit = int(step.args["n"])
        elif op == "distinct":
            distinct = True
        elif op == "sample":
            sample = (int(step.args["n"]), step.args.get("seed"))
        elif op == "rename":
            rename.update(step.args["mapping"])
        elif op == "select":
            if select is not None:
                raise NotImplementedError("collect() supports a single select() step for now.")
            select = _parse_select(step.args["columns"])
        else:
            raise NotImplementedError(
                f"collect() does not yet support the '{op}' step on an edge frame."
            )

    if group_by is not None and (distinct or sample is not None or select is not None):
        raise NotImplementedError(
            "group_by().agg() does not compose with distinct/sample/select yet; "
            "sort/head/rename on the grouped result are supported."
        )

    if join is not None:
        if traversal is not None:
            raise NotImplementedError(
                "join() on a traversal result (hop/shortest_path) is not supported yet; "
                "join is available on plain edge/node frames."
            )
        if group_by is not None:
            raise NotImplementedError(
                "join() does not compose with group_by().agg() yet; do them in "
                "separate collect() steps."
            )
        left_table = _edge_source_table(frame)
        return _run_join(
            left_table,
            join,
            filters,
            sort,
            limit,
            select,
            sample,
            rename,
            distinct,
            {"src": frame.src_col, "dst": frame.dst_col},
        )

    if traversal is None:
        # No traversal: a plain edge frame (scan_edges / from_arrow / read_edges,
        # or a reversed frame). Materialize its edge rows + tail (distinct included).
        return _collect_plain_edges(
            frame, filters, sort, limit, select, sample, rename, distinct, group_by
        )
    if group_by is not None:
        raise NotImplementedError(
            "group_by().agg() on a traversal result (hop/shortest_path) is not "
            "supported yet; group_by is available on plain edge/node frames."
        )

    # A traversal result carries reserved `src`/`dst` columns (+ a path cost for
    # weighted shortest_path); ur.src()/dst() in a filter resolve to them.
    filter_json = [_filter_json(p, {"src": "src", "dst": "dst"}) for p in filters]
    rename_pairs = list(rename.items())
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
            sample,
            rename_pairs,
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
            sample,
            rename_pairs,
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
    distinct: bool = False,
    sample: tuple[int, int | None] | None = None,
    rename: dict[str, str] | None = None,
    group_by: tuple[list[str], list[str], list[dict[str, str]]] | None = None,
) -> MaterializedFrame:
    index = _require_index(edges)
    # The node attribute table crosses the FFI as a batch list (not one batch).
    nodes_batches = nodes.to_batches() if nodes is not None else None
    # A node-query result carries the reserved `id` column plus the computed/joined
    # columns; `ur.id()` in a filter resolves to it. (src/dst have no role here.)
    filter_json = [_filter_json(p, {"id": "id"}) for p in filters]
    group_keys, engine_aggs = ([], []) if group_by is None else (group_by[0], group_by[1])
    batches = _native().run_node_query(
        index,
        json.dumps(columns),
        filter_json,
        sort,
        limit,
        nodes_batches,
        nodes_id,
        edge_attr,
        distinct,
        sample,
        list((rename or {}).items()),
        group_keys,
        engine_aggs,
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


def _reject_derived_edge_graph_op(edges: Any) -> None:
    """Reject a graph op on a *derived* edge frame — one whose plan contains a
    row-changing op (filter/sample/distinct/join/group_by) or is a traversal result
    (hop/shortest_path). Such a frame carries no topology that matches its rows (its
    retained source is the *pre*-derivation edges), so building a CSR from it would
    be silently wrong.

    This is the single **authoritative** guard: it inspects only the plan, so it
    must be called *before* the frame's index cell is read or (re)built — a cell
    populated by another path (a weighted scan's pre-build, or a stale shared cell)
    would otherwise smuggle a wrong topology past a build-time-only check. The
    plain-collect path materializes a derived frame's rows separately (via
    `_edge_source_table`), so `edges.filter(...).collect()` still works (#100)."""
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


def _require_edges(edges: EdgeFrame | None) -> list[Any]:
    """Resolve an EdgeFrame to a **batch list** of ``[src, dst]`` RecordBatches
    (endpoints canonicalized to int64 or string) ready for ``build_index``, or raise
    clearly. The list is fed to the CSR build as a stream — never concatenated into
    one contiguous batch first (#60)."""
    if edges is None:
        raise NotImplementedError("this expression is not associated with an edge frame.")
    _reject_derived_edge_graph_op(edges)

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
    # Guard *before* touching the cell: a graph op on a derived frame must raise even
    # if the cell was pre-populated elsewhere (Bug: a weighted scan builds the index
    # directly; a grouped frame may share a parent's built cell).
    _reject_derived_edge_graph_op(edges)
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


# --- group_by().agg() ------------------------------------------------------
_AGG_FNS = {"mean", "sum", "min", "max", "count", "n_unique"}


def _reject_pre_group_tail(
    sort: tuple[str, bool] | None, limit: int | None, rename: dict[str, str]
) -> None:
    """The canonical tail applies sort/head/rename *after* the grouping, so a
    sort/head/rename written *before* group_by().agg() would be silently
    reordered (e.g. ``.head(10).group_by(...)`` would truncate groups, not rows).
    Reject it rather than execute a different query than the one written; these
    verbs are supported *after* the group."""
    offenders = [
        name
        for name, present in (
            ("sort", sort is not None),
            ("head", limit is not None),
            ("rename", bool(rename)),
        )
        if present
    ]
    if offenders:
        raise NotImplementedError(
            f"{'/'.join(offenders)}() before group_by().agg() is not supported "
            "(the tail applies them after the group); move them after the group_by."
        )


def _parse_group_keys(keys: Any) -> list[str]:
    """The group key column names. Accepts bare strings or ``ur.col(name)``."""
    out: list[str] = []
    for k in keys:
        if isinstance(k, str):
            out.append(k)
        elif getattr(k, "kind", None) == "col":
            out.append(k.payload["name"])
        else:
            raise NotImplementedError("group_by() supports column names or ur.col(<name>) as keys.")
    if not out:
        raise NotImplementedError("group_by() needs at least one key column.")
    return out


def _parse_agg_expr(name: str | None, expr: Any) -> tuple[str, str, str]:
    """Resolve one aggregation to ``(output_name, fn, column)``.

    Accepts ``ur.col(c).<fn>()`` optionally wrapped in ``.alias(x)``; the output
    name is (highest precedence first) the ``.agg(name=...)`` keyword, then a
    ``.alias()``, then the default ``{column}_{fn}``. An arithmetic or non-``col``
    aggregand, or a non-aggregation expression, is a clear error (deferred)."""
    alias_name: str | None = None
    node = expr
    if getattr(node, "kind", None) == "alias":
        alias_name = node.payload["name"]
        node = node.payload["operand"]
    if getattr(node, "kind", None) != "agg":
        raise NotImplementedError(
            "agg() expects an aggregation like ur.col('x').mean() "
            "(fns: mean/sum/min/max/count/n_unique)."
        )
    fn = node.payload["fn"]
    if fn not in _AGG_FNS:
        raise NotImplementedError(
            f"aggregation {fn!r} is not supported (use {', '.join(sorted(_AGG_FNS))})."
        )
    operand = node.payload["operand"]
    if getattr(operand, "kind", None) != "col":
        raise NotImplementedError(
            "agg() supports aggregating a single ur.col(<name>) for now "
            "(aggregating an expression like col('a') * col('b') is future work)."
        )
    column = operand.payload["name"]
    output = name or alias_name or f"{column}_{fn}"
    return (output, fn, column)


def _parse_group_agg(step: _PlanStep) -> tuple[list[str], list[str], list[dict[str, str]]]:
    """Parse a ``group_by_agg`` step into ``(keys, engine_aggs, plain_aggs)`` — the
    group keys, the serialized ``alias(agg(col))`` JSON per aggregation (engine
    path), and ``{fn, column, output}`` dicts (pyarrow plain path). The two agg
    encodings resolve to the *same* output column names, so both paths agree."""
    keys = _parse_group_keys(step.args["keys"])
    resolved: list[tuple[str, str, str]] = []
    for expr in step.args["exprs"]:  # positional aggregations
        resolved.append(_parse_agg_expr(None, expr))
    for out_name, expr in step.args["named"].items():  # .agg(name=...)
        resolved.append(_parse_agg_expr(out_name, expr))
    if not resolved:
        raise NotImplementedError("group_by().agg() needs at least one aggregation.")
    seen: set[str] = set()
    for output, _fn, _col in resolved:
        if output in keys:
            raise ValueError(f"aggregation output name {output!r} collides with a group key.")
        if output in seen:
            raise ValueError(f"duplicate aggregation output name {output!r}.")
        seen.add(output)
    engine_aggs = [
        json.dumps(
            {
                "kind": "alias",
                "name": output,
                "operand": {"kind": "agg", "fn": fn, "operand": {"kind": "col", "name": col}},
            }
        )
        for (output, fn, col) in resolved
    ]
    plain_aggs = [{"fn": fn, "column": col, "output": output} for (output, fn, col) in resolved]
    return keys, engine_aggs, plain_aggs


def _parse_sort(args: dict[str, Any]) -> tuple[str, bool]:
    by = args["by"]
    if not isinstance(by, str):
        raise NotImplementedError(
            "collect() sort supports a single column name for now "
            "(e.g. .sort('pagerank', descending=True))."
        )
    return (by, bool(args.get("descending", False)))


# --- join ------------------------------------------------------------------
_JOIN_HOWS = {"inner", "left"}


def _parse_join_keys(on: Any) -> list[str]:
    """The join key column name(s). Accepts a bare string, ``ur.col(name)``, or a
    list of either. ``on=None`` (key inference) is deferred with a clear error."""
    if on is None:
        raise NotImplementedError(
            "join() needs on= (key inference is not supported yet); pass on='id' or on=['a', 'b']."
        )
    items = on if isinstance(on, (list, tuple)) else [on]
    keys: list[str] = []
    for k in items:
        if isinstance(k, str):
            keys.append(k)
        elif getattr(k, "kind", None) == "col":
            keys.append(k.payload["name"])
        else:
            raise NotImplementedError("join(on=): keys must be column names or ur.col(<name>).")
    if not keys:
        raise NotImplementedError("join() needs at least one key column.")
    return keys


def _parse_join(step: _PlanStep) -> tuple[Any, list[str], str]:
    """Parse a ``join`` step into ``(other, on_keys, how)``. ``other`` must be an
    ursa frame; ``how`` is restricted to the supported set."""
    from ._frames import _Frame

    other = step.args["other"]
    if not isinstance(other, _Frame):
        raise TypeError(
            "join(other): other must be an ursa frame (EdgeFrame/NodeFrame); "
            f"got {type(other).__name__!r}."
        )
    how = step.args["how"]
    if how not in _JOIN_HOWS:
        raise NotImplementedError(
            f"join how={how!r} is not supported yet (use {sorted(_JOIN_HOWS)}); "
            "'right'/'outer' are a follow-up."
        )
    return other, _parse_join_keys(step.args["on"]), how


def _run_join(
    left_table: Any,
    join: tuple[Any, list[str], str],
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    select: list[str] | None,
    sample: tuple[int, int | None] | None,
    rename: dict[str, str],
    distinct: bool,
    roles: dict[str, str],
) -> MaterializedFrame:
    """Materialize the right frame, validate keys/collisions, and run the native
    join + relational tail. The whole tail (filter/sort/head/distinct/sample/
    rename) applies *after* the join; ``roles`` resolves ur.id()/src()/dst() in a
    filter to the left frame's real columns."""
    other, on_keys, how = join
    try:
        right_table = other.collect().to_arrow()
    except NotImplementedError as exc:
        raise NotImplementedError(
            f"join(other): could not materialize the right-hand frame ({exc})."
        ) from exc
    left_cols, right_cols = left_table.column_names, right_table.column_names
    for k in on_keys:
        if k not in left_cols:
            from . import ColumnNotFoundError

            raise ColumnNotFoundError(f"join(on=): key {k!r} not in the left frame {left_cols}.")
        if k not in right_cols:
            from . import ColumnNotFoundError

            raise ColumnNotFoundError(f"join(on=): key {k!r} not in the right frame {right_cols}.")
    # A same-named non-key column on both sides would collide in the output; reject
    # it (a polars-style `_right` suffix is a follow-up).
    collide = (set(left_cols) - set(on_keys)) & (set(right_cols) - set(on_keys))
    if collide:
        raise ValueError(
            f"join produces duplicate column name(s) {sorted(collide)}; "
            "rename() one side before joining."
        )
    filter_json = [_filter_json(p, roles) for p in filters]
    batches = _native().run_join_query(
        left_table.to_batches(),
        right_table.to_batches(),
        on_keys,
        how,
        filter_json,
        sort,
        limit,
        distinct,
        sample,
        list((rename or {}).items()),
    )
    return _apply_select(MaterializedFrame(batches), select)


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


# A plain frame takes exactly one deterministic sampling policy, distinct from the
# engine's (both reproducible, not byte-identical to each other): a stable stdlib
# PRNG over the single in-memory table's row order (which, unlike the engine, is
# unpartitioned, so no content-canonical ordering is needed for thread-invariance).
_DEFAULT_SAMPLE_SEED = 0x5EED5EED


def _sample_pyarrow(table: Any, n: int, seed: int | None) -> Any:
    """Deterministically take ``n`` rows (without replacement) from a pyarrow table.
    ``n >= row_count`` returns all rows; ``seed=None`` uses the default seed."""
    import random

    r = table.num_rows
    if n >= r:
        return table
    idx = random.Random(seed if seed is not None else _DEFAULT_SAMPLE_SEED).sample(range(r), n)
    return table.take(sorted(idx))


def _apply_pyarrow_rename(table: Any, rename: dict[str, str]) -> Any:
    """Relabel output columns of a pyarrow table. An unknown source column is an
    error (no silent no-op), matching the engine path's ``rename`` check."""
    if not rename:
        return table
    missing = [old for old in rename if old not in table.column_names]
    if missing:
        from . import ColumnNotFoundError

        raise ColumnNotFoundError(f"rename() references unknown column(s): {missing}")
    return table.rename_columns([rename.get(c, c) for c in table.column_names])


# pyarrow.compute aggregate function names by our fn name.
_PC_AGG = {
    "mean": "mean",
    "sum": "sum",
    "min": "min",
    "max": "max",
    "count": "count",
    "n_unique": "count_distinct",
}


def _apply_pyarrow_group_by(table: Any, keys: list[str], aggs: list[dict[str, str]]) -> Any:
    """Group ``table`` by ``keys`` and aggregate, producing `[keys..., outputs...]`
    with the same output column names as the engine path. Mirrors the engine's
    unknown-key error."""
    for k in keys:
        if k not in table.column_names:
            raise ValueError(f"group_by() references unknown column '{k}'.")
    for a in aggs:
        if a["column"] not in table.column_names:
            raise ValueError(f"agg() references unknown column '{a['column']}'.")
    # Deduplicate the (column, pcfn) computations: two aggregations with distinct
    # output names but the same column+fn (e.g. .agg(a=col('x').sum(), b=col('x')
    # .sum())) share one pyarrow aggregate, and the engine path likewise emits two
    # aliases of one sum — so both paths stay consistent. pyarrow names an
    # aggregate output "{column}_{pcfn}"; each is mapped back to *every* output
    # that requested it.
    specs = []
    seen_specs: set[tuple[str, str]] = set()
    for a in aggs:
        spec = (a["column"], _PC_AGG[a["fn"]])
        if spec not in seen_specs:
            seen_specs.add(spec)
            specs.append(spec)
    grouped = table.combine_chunks().group_by(keys).aggregate(specs)
    cols = {name: grouped.column(name) for name in grouped.column_names}
    import pyarrow as pa

    ordered_names = list(keys)
    ordered_cols = [cols[k] for k in keys]
    for a in aggs:
        ordered_names.append(a["output"])
        ordered_cols.append(cols[f"{a['column']}_{_PC_AGG[a['fn']]}"])
    return pa.table(dict(zip(ordered_names, ordered_cols, strict=True)))


def _apply_pyarrow_tail(
    table: Any,
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    roles: dict[str, str],
    sample: tuple[int, int | None] | None = None,
    rename: dict[str, str] | None = None,
    distinct: bool = False,
    group_by: tuple[list[str], list[str], list[dict[str, str]]] | None = None,
) -> Any:
    # Same canonical order as the engine tail:
    # filters → group_by → distinct → sample → sort → limit → rename.
    for predicate in filters:
        _require_predicate(predicate)  # same top-of-predicate check as the graph path
        table = table.filter(_eval_pyarrow(predicate, table, roles))
    if group_by is not None:
        table = _apply_pyarrow_group_by(table, group_by[0], group_by[2])
    if distinct:
        # Distinct rows over all columns (group-by with no aggregations); row order
        # is unspecified for distinct, as on the engine path.
        table = table.combine_chunks().group_by(table.column_names).aggregate([])
    if sample is not None:
        table = _sample_pyarrow(table, sample[0], sample[1])
    if sort is not None:
        by, descending = sort
        if by not in table.column_names:
            raise ValueError(f"sort references unknown column '{by}'.")
        table = table.sort_by([(by, "descending" if descending else "ascending")])
    if limit is not None:
        table = table.slice(0, limit)
    if rename:
        table = _apply_pyarrow_rename(table, rename)
    return table


def _collect_plain_nodes(
    frame: NodeFrame,
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    select: list[str] | None = None,
    sample: tuple[int, int | None] | None = None,
    rename: dict[str, str] | None = None,
    distinct: bool = False,
    group_by: tuple[list[str], list[str], list[dict[str, str]]] | None = None,
) -> MaterializedFrame:
    # On a plain node table, ur.id() resolves to the frame's real id column.
    roles = {"id": frame.id_col}
    # With no computed columns, the scan need only read the id, whatever the
    # filter/sort touch, any rename source column, and (if a select narrows the
    # output) the selected columns. A rename *target* is a new label, not a file
    # column, so it must not leak into the projection. When group_by is present the
    # whole file is read (keys + agg columns must be available; select is disallowed).
    file_needed: list[str] | None = None
    if select is not None and group_by is None:
        rename = rename or {}
        referenced: set[str] = set(rename)
        for p in filters:
            referenced |= _expr_columns(p, roles)
        if sort is not None:
            referenced.add(sort[0])
        selected_file = set(select) - set(rename.values())
        file_needed = sorted({frame.id_col} | referenced | selected_file)
    attr = _resolve_node_attr_table(frame, file_needed)
    if attr is None:
        raise NotImplementedError(
            "collect() on this NodeFrame needs a source (scan_nodes / from_arrow(id=...)) "
            "or a with_columns of graph algorithms; a bare edges.nodes() has no "
            "materializable node set yet."
        )
    # `attr` is a (chunked) pyarrow.Table.
    table = _apply_pyarrow_tail(
        attr, filters, sort, limit, roles, sample, rename, distinct, group_by
    )
    return _apply_select(MaterializedFrame(table), select)


def _collect_plain_edges(
    frame: EdgeFrame,
    filters: list[Any],
    sort: tuple[str, bool] | None,
    limit: int | None,
    select: list[str] | None = None,
    sample: tuple[int, int | None] | None = None,
    rename: dict[str, str] | None = None,
    distinct: bool = False,
    group_by: tuple[list[str], list[str], list[dict[str, str]]] | None = None,
) -> MaterializedFrame:
    # On a plain edge table, ur.src()/dst() resolve to the frame's real endpoint cols.
    roles = {"src": frame.src_col, "dst": frame.dst_col}
    table = _edge_source_table(frame)
    table = _apply_pyarrow_tail(
        table, filters, sort, limit, roles, sample, rename, distinct, group_by
    )
    return _apply_select(MaterializedFrame(table), select)


def _edge_source_table(frame: EdgeFrame) -> Any:
    """The raw edge rows of a plain (non-traversal) edge frame as a pyarrow.Table —
    the full in-memory edge table, or a fresh scan of its ``scan_edges`` source.
    Raises the precise plan-aware error for a frame with no materializable source."""
    import pyarrow as pa

    table = getattr(frame, "_edge_attr_table", None)  # full in-memory edge table
    if table is not None:
        return table
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
        return pa.Table.from_batches(batches).rename_columns([scan["src"], scan["dst"]])
    # No source: a filtered/derived (or traversal-result) edge frame.
    _require_edges(frame)  # raises the precise, plan-aware message
    raise AssertionError("unreachable")  # _require_edges always raises here


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
