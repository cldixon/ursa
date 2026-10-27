"""Executing a frame — Ursa's ``collect()``.

``collect()`` serializes the frame's plan to the Rust plan IR
(``ursa_plan::plan::Plan``) and hands it to the one native entry point,
``ursa._ursa.run_plan``. Rust decides how the plan runs: it walks the tree, rejects
the shapes that aren't supported, and maps the rest onto its executors.

What stays here is leaf work that has not moved to Rust yet. ``run_plan`` reads it
back through ``_Host``, by slot, as it needs it:

- the per-frame caches: the topology index, subgraph-view edge masks, scanned node
  attribute tables (all memoized on ``_frames._BuildCell``);
- in-memory tables, plain-source scans, and the edge columns a weight reads;
- seed id sets;
- the pyarrow evaluator for the relational steps the engine's fixed-order tail
  can't run as written, and for every step on a plain source (#191 retires it).
"""

from __future__ import annotations

import json
import math
from typing import TYPE_CHECKING, Any

from ._result import MaterializedFrame

if TYPE_CHECKING:
    from ._frames import EdgeFrame, NodeFrame, _Frame, _PlanStep

# The lazy index build is guarded per-frame by the lock on each frame's shared
# `_BuildCell` (see `_frames._BuildCell`), so first-collects of *unrelated* frames
# from different threads build concurrently instead of serializing on one global
# lock. collect() runs GIL-free, so the memo assignment itself still needs the
# cell's lock.

# Operators valid at the *top* of a filter predicate — comparisons and boolean
# combinators. (Arithmetic ops may appear deeper, e.g. `(a + b) > 3`, but a bare
# arithmetic or column expression is not itself a boolean predicate.)
_PREDICATE_OPS = {">", "<", ">=", "<=", "==", "!=", "&", "|"}


# --- collect ---------------------------------------------------------------
def collect(frame: _Frame) -> MaterializedFrame:
    """Execute ``frame``: serialize its plan, run it, wrap the result."""
    host = _Host()
    plan = host.frame(frame)
    return MaterializedFrame(_native().run_plan(_dumps(plan), host))


def _graph_index(edges: EdgeFrame) -> Any:
    """The checked graph index of ``edges``, for the eager whole-graph stats."""
    host = _Host()
    return _native().resolve_index(_dumps(host.edges(edges)), host)


def _dumps(plan: Any) -> str:
    return json.dumps(plan, default=_json_default)


def _json_default(obj: Any) -> Any:
    """JSON for the numeric scalars numpy and friends pass as parameters."""
    if hasattr(obj, "__index__"):
        return obj.__index__()
    if hasattr(obj, "__float__"):
        return float(obj)
    raise TypeError(f"Object of type {type(obj).__name__} is not JSON serializable")


class _Host:
    """One plan's inputs, by slot: the serializer's side table and the callbacks
    ``run_plan`` makes (``ursa_plan::plan::Inputs``).

    A slot is a frame (whose source, caches and masks live on the Python object)
    or a seed set. A relational node of the plan also carries the handle of the
    ``_PlanStep`` it came from, so the steps the engine can't run are applied
    here from the original step, with no round trip through JSON."""

    __slots__ = ("_frame_slots", "_slots", "_steps")

    def __init__(self) -> None:
        self._slots: list[Any] = []
        self._frame_slots: dict[int, int] = {}
        self._steps: list[_PlanStep] = []

    # -- serialization ---------------------------------------------------------
    def frame(self, frame: _Frame) -> dict[str, Any]:
        """The plan tree of ``frame``: each step wraps the one before it."""
        slot = self._frame_slot(frame)
        node: dict[str, Any] | None = None
        for step in frame._plan:
            node = self._step(step, node, slot)
        assert node is not None, "a frame's plan starts with its source"
        return node

    def edges(self, edges: EdgeFrame | None) -> dict[str, Any] | None:
        """An edge input: the edge frame's plan and its slot."""
        if edges is None:
            return None
        return {"plan": self.frame(edges), "frame": self._frame_slot(edges)}

    def _frame_slot(self, frame: Any) -> int:
        slot = self._frame_slots.get(id(frame))
        if slot is None:
            slot = self._frame_slots[id(frame)] = self._slot(frame)
        return slot

    def _slot(self, value: Any) -> int:
        self._slots.append(value)
        return len(self._slots) - 1

    def _handle(self, step: _PlanStep) -> int:
        self._steps.append(step)
        return len(self._steps) - 1

    def _step(self, step: _PlanStep, input: Any, slot: int) -> dict[str, Any]:
        op, a = step.op, step.args
        if op == "scan_edges":
            return {
                "op": op,
                "frame": slot,
                "path": _scan_path(a["path"]),
                "src": a["src"],
                "dst": a["dst"],
                "on_null": a["on_null"],
            }
        if op == "scan_nodes":
            return {"op": op, "frame": slot, "path": _scan_path(a["path"]), "id": a["id"]}
        if op in ("from_arrow", "from_polars"):
            roles = {k: a[k] for k in ("src", "dst", "id") if k in a}
            return {"op": "from_arrow", "frame": slot, **roles}
        if op in ("nodes", "reverse"):
            return {"op": op, "input": input}
        if op == "hop":
            return {
                "op": op,
                "edges": self.edges(a["edges"]),
                "n": int(a["n"]),
                "direction": a["direction"],
                "seeds": self._slot(a["seeds"]),
            }
        if op == "shortest_path":
            weight = a.get("weight")
            return {
                "op": op,
                "edges": self.edges(a["edges"]),
                # source/target resolve like hop seeds, as 1-element id sets.
                "source": self._slot([a["source"]]),
                "target": self._slot([a["target"]]),
                "weight": None if weight is None else _expr_to_json(weight),
                "direction": a["direction"],
            }
        if op == "random_walk":
            return {
                "op": op,
                "edges": self.edges(a["edges"]),
                "start": self._slot(a["start"]),
                "steps": int(a["steps"]),
                "walks_per_node": int(a["walks_per_node"]),
                "seed": a.get("seed"),
            }
        if op == "describe":
            return {
                "op": op,
                "edges": self.edges(a["edges"]),
                "full": bool(a.get("full", False)),
                "direction": a.get("direction", "out"),
            }
        if op == "with_columns":
            columns = [{"name": n, "expr": self._expr(e)} for n, e in a["exprs"].items()]
            return {"op": op, "input": input, "columns": columns}
        if op == "join":
            from ._frames import _Frame

            other = a["other"]
            if not isinstance(other, _Frame):
                raise TypeError(
                    "join(other): other must be an ursa frame (EdgeFrame/NodeFrame); "
                    f"got {type(other).__name__!r}."
                )
            try:
                right = self.frame(other)
            except NotImplementedError as exc:
                raise NotImplementedError(
                    f"join(other): could not materialize the right-hand frame ({exc})."
                ) from exc
            on = _parse_join_keys(a["on"])
            return {"op": op, "input": input, "right": right, "on": on, "how": a["how"]}
        node: dict[str, Any] = {"op": op, "input": input, "step": self._handle(step)}
        if op == "filter":
            node["predicate"] = _expr_to_json(a["predicate"])
        elif op == "select":
            node["columns"] = _parse_select(a["columns"])
        elif op == "rename":
            node["mapping"] = [[old, new] for old, new in dict(a["mapping"]).items()]
        elif op == "sort":
            node["by"] = [{"column": c, "descending": d} for c, d in _parse_sort(a)]
        elif op == "head":
            node["n"] = int(a["n"])
        elif op == "sample":
            node.update(n=int(a["n"]), seed=a.get("seed"))
        elif op == "group_by_agg":
            keys, aggs = _parse_group_agg(step)
            node.update(keys=keys, aggs=aggs)
        # distinct: no arguments. An unknown op fails the plan's parse.
        return node

    def _expr(self, expr: Any) -> dict[str, Any]:
        """A with_columns expression: a graph verb with its edge input, or any
        other expression (which the engine rejects with a clear error)."""
        if getattr(expr, "kind", None) != "graph":
            return _expr_to_json(expr) if hasattr(expr, "kind") else {"kind": "unknown"}
        out: dict[str, Any] = {"kind": "graph"}
        for key, value in expr.payload.items():
            if key == "edges":
                out[key] = self.edges(value)
            elif key in ("weight", "agg"):
                out[key] = None if value is None else _expr_to_json(value)
            elif key == "from_":
                out[key] = value is not None
            else:
                out[key] = value
        return out

    # -- inputs (called back from run_plan) ------------------------------------
    def index(self, slot: int) -> Any:
        return _require_index(self._slots[slot])

    def edge_mask(self, slot: int) -> Any | None:
        frame = self._slots[slot]
        return _edge_mask_for(frame, _require_index(frame))

    def edge_weights(self, slot: int, columns: list[str], keys: list[str]) -> Any | None:
        return _prepare_weighted(self._slots[slot], columns, keys)

    def node_table(self, slot: int, columns: list[str] | None) -> list[Any]:
        return _batches(_resolve_node_attr_table(self._slots[slot], columns))

    def edge_table(self, slot: int, columns: list[str] | None) -> list[Any]:
        return _batches(_edge_source_table(self._slots[slot], columns))

    def seeds(self, slot: int) -> Any:
        return _resolve_seeds(self._slots[slot])

    def apply_steps(self, batches: list[Any], handles: list[int], roles: dict[str, str]) -> Any:
        import pyarrow as pa

        steps = [_tail_step(self._steps[h]) for h in handles]
        return _batches(_apply_pyarrow_steps(pa.Table.from_batches(batches), steps, roles))


def _batches(table: Any) -> list[Any]:
    """A table as a batch list that keeps its schema when empty (#153)."""
    import pyarrow as pa

    return table.to_batches() or [pa.RecordBatch.from_pylist([], schema=table.schema)]


def _scan_path(path: Any) -> Any:
    """A scan path for the plan, when representable (the host keeps the spec)."""
    if isinstance(path, str):
        return path
    if isinstance(path, (list, tuple)) and all(isinstance(p, str) for p in path):
        return list(path)
    return None


# --- expression serialization ----------------------------------------------
def _expr_to_json(expr: Any) -> dict[str, Any]:
    """Serialize an ``Expr`` tree to the JSON grammar of ``ursa_plan::expr``:
    ``col`` / ``lit`` / ``binary`` / ``unary`` / ``agg`` / ``alias``, and the role
    references ``src`` / ``dst`` / ``id``, which the executor resolves to columns
    for the context the expression runs in."""
    kind, p = expr.kind, expr.payload
    if kind == "col":
        return {"kind": "col", "name": p["name"]}
    if kind == "lit":
        return {"kind": "lit", "value": _lit_json(p["value"])}
    if kind == "binary":
        return {
            "kind": "binary",
            "op": p["op"],
            "left": _expr_to_json(p["left"]),
            "right": _expr_to_json(p["right"]),
        }
    if kind == "unary":
        return {"kind": "unary", "op": p["op"], "operand": _expr_to_json(p["operand"])}
    if kind == "agg":
        return {"kind": "agg", "fn": p["fn"], "operand": _expr_to_json(p["operand"])}
    if kind == "alias":
        return {"kind": "alias", "name": p["name"], "operand": _expr_to_json(p["operand"])}
    return {"kind": kind}


def _lit_json(value: Any) -> Any:
    """A literal as JSON. A value JSON can't hold (NaN, a datetime, ...) becomes
    null: the engine rejects it with a clear error, while the host's evaluator
    reads the original from the step."""
    if value is None or isinstance(value, (bool, str, int)):
        return value
    if hasattr(value, "__index__"):
        return value.__index__()
    if isinstance(value, float) or hasattr(value, "__float__"):
        f = float(value)
        return f if math.isfinite(f) else None
    return None


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


def _select_table(table: Any, select: list[str]) -> Any:
    """Narrow a pyarrow table to the selected columns (the ``select`` output
    projection). ``select`` never changes rows, only which columns are kept and in
    what order — the Polars-faithful counterpart to additive ``with_columns``."""
    missing = [c for c in select if c not in table.column_names]
    if missing:
        from . import ColumnNotFoundError

        raise ColumnNotFoundError(f"select() references unknown column(s): {missing}")
    return table.select(select)


def _prepare_weighted(
    edges: EdgeFrame, weight_columns: list[str] | set[str], weight_jsons: list[str] | None = None
) -> Any | None:
    """The edge attribute table (a **list** of pyarrow RecordBatches) for evaluating
    a weight expression, aligned to the graph's edge-row order.

    The edge table crosses the FFI as a batch list (never concatenated into one
    contiguous batch, #60); ``evaluate_weight`` streams the batches in order, and
    that order matches the topology's ``edge_ids`` because the *same* row order
    built the index.

    ``weight_jsons`` (the serialized expressions, for a node query) lets this
    return ``None`` when the frame's built index already caches every one of those
    weight vectors (#175): the engine reuses them without reading the edge table.

    For a ``scan_edges`` frame this scans ``src``, ``dst`` and the weight columns
    together, dropping null-endpoint rows under ``on_null="drop"`` exactly as the
    index build does. Scans are file-ordered and deterministic (#148), so these rows
    align with an index already built from an earlier scan, which is reused rather
    than rebuilt (its cached views and weights survive); with no index yet, it is
    built from these same batches, saving a second scan. In-memory frames already
    share one retained table, so they just subset it."""
    if not weight_columns:
        return None
    # (The executor has already rejected an edge plan a graph op can't run over, so
    # the scan branch never builds the index of a reshaped frame.)
    # A traversal-result frame (#116) carries no source of its own — the weight columns
    # and the topology index both live on the parent, so resolve to it (the parent cell
    # is the one `_require_index` reads, keeping weights and edge_ids self-consistent).
    edges = _parent_edges(edges)
    index = edges._index_build_cell.value
    if weight_jsons and index is not None and all(index.has_weights(j) for j in weight_jsons):
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
        # One scan → a batch list of [src, dst, *cols], filtered like the index
        # build so its rows match the index's edge rows one for one. A built index
        # is kept; otherwise these batches' endpoints build it.
        combined = _native().scan_edges_arrow(
            path, scan["src"], scan["dst"], _scan_storage_options(scan), cols
        )
        if scan.get("on_null") == "drop":
            combined = _drop_null_endpoint_batches(combined)
        cell = edges._index_build_cell
        if cell.value is None:
            with cell.lock:
                if cell.value is None:
                    cell.value = _native().build_index(combined)
        return combined
    raise NotImplementedError(
        "a weighted algorithm needs an in-memory or scan_edges source so the weight "
        "columns can be read."
    )


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
        return inmem  # already a chunked Table (see _io._node_attr_table)
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


_TRAVERSAL_OPS = ("hop", "shortest_path")


def _traversal_step(frame: Any) -> Any | None:
    """The ``hop``/``shortest_path`` step in a traversal EdgeFrame's plan, or ``None``.
    A traversal frame carries exactly one such step (see ``_graph._Hop`` /
    ``_graph.shortest_path``); the parent edges ride in ``step.args['edges']``."""
    for step in getattr(frame, "_plan", ()):
        if step.op in _TRAVERSAL_OPS:
            return step
    return None


def _parent_edges(frame: Any) -> Any:
    """The parent edge frame a graph op actually runs over. For a traversal-result
    frame (#116) that is the frame the traversal was built from (carried in the
    step) — its CSR is the topology, and the traversal only *masks* it. For any
    other frame it is the frame itself."""
    step = _traversal_step(frame)
    return step.args["edges"] if step is not None else frame


def _drop_null_endpoint_batches(
    batches: list[Any], endpoints: tuple[int | str, int | str] = (0, 1)
) -> list[Any]:
    """Filter scan batches to rows with a non-null src and dst, emitting a count —
    the scan half of the ``on_null="drop"`` opt-in. ``endpoints`` locates the two
    columns: positions 0/1 for the graph-op scans (endpoints first, weight columns
    after), or the endpoint column names for the full-table scan of the relational
    paths. Filtering here (rather than in a second pass) keeps the topology and any
    co-scanned weights row-aligned, since they come from the same filtered batches."""
    import pyarrow.compute as pc

    from ._io import _warn_dropped_endpoints

    # pyarrow.compute members are looked up by name (the type checker lacks its stubs).
    is_valid, and_ = pc.is_valid, pc.and_  # ty: ignore[unresolved-attribute]
    out, dropped = [], 0
    for b in batches:
        mask = and_(is_valid(b.column(endpoints[0])), is_valid(b.column(endpoints[1])))
        kept = b.filter(mask)
        dropped += b.num_rows - kept.num_rows
        out.append(kept)
    _warn_dropped_endpoints(dropped)
    return out


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

        # Zero-copy batches over the chunked endpoints (chunk boundaries that
        # differ between the two columns are sliced, not copied).
        table = pa.Table.from_arrays(list(arrays), names=["src", "dst"])
        return table.to_batches() or [
            pa.RecordBatch.from_arrays(
                [col.combine_chunks() for col in table.columns], ["src", "dst"]
            )
        ]

    scan = getattr(edges, "_scan_spec", None)
    if scan is not None:
        path = scan["path"]
        if not isinstance(path, str):
            raise NotImplementedError(
                "collect() over a scan_edges source supports a single string path "
                "(glob included) for now, not a list of paths."
            )
        # The scan returns a list of [src, dst] batches; build_index reads columns
        # 0/1 of each, so it is passed straight through (minus null-endpoint rows
        # when on_null="drop").
        batches = _native().scan_edges_arrow(
            path, scan["src"], scan["dst"], _scan_storage_options(scan)
        )
        if scan.get("on_null") == "drop":
            batches = _drop_null_endpoint_batches(batches)
        return batches

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
    # (The executor checks the edge plan before it asks for the index: see
    # ursa_plan::plan::require_index.)
    # A traversal-result frame (#116) has no source of its own — its CSR is the
    # parent's, and the traversal only masks it. Resolve to the parent so the parent
    # CSR is built once and shared with direct ops on that frame (index-preservation).
    parent = _parent_edges(edges)
    cell = parent._index_build_cell
    idx = cell.value
    if idx is not None:
        return idx
    with cell.lock:
        idx = cell.value
        if idx is None:
            idx = _scan_build_index(parent)
            if idx is None:
                idx = _native().build_index(_require_edges(parent))
            cell.value = idx
    return idx


def _scan_build_index(edges: EdgeFrame) -> Any:
    """Build the index for a single-path ``scan_edges`` frame in one native call, or
    return ``None`` for any other source. The scanned endpoints stay in Rust and are
    freed batch by batch as they are interned, instead of a Python batch list holding
    all of them until the CSR is built (#149). Rows are the same as
    ``_require_edges`` yields, so the index is identical."""
    scan = getattr(edges, "_scan_spec", None)
    if scan is None or not isinstance(scan["path"], str):
        return None
    from ._io import _warn_dropped_endpoints

    idx, dropped = _native().scan_build_index(
        scan["path"],
        scan["src"],
        scan["dst"],
        _scan_storage_options(scan),
        scan.get("on_null") == "drop",
    )
    _warn_dropped_endpoints(dropped)
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


# --- group_by().agg() ------------------------------------------------------
_AGG_FNS = {"mean", "sum", "min", "max", "count", "n_unique"}


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


def _parse_group_agg(step: _PlanStep) -> tuple[list[str], list[dict[str, str]]]:
    """Parse a ``group_by_agg`` step into ``(keys, aggs)``: the group keys and one
    ``{output, fn, column}`` per aggregation, the shape of the plan's
    ``group_by_agg`` node (which the engine lowers to ``alias(agg(col))``)."""
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
    return keys, [{"output": output, "fn": fn, "column": col} for (output, fn, col) in resolved]


def _parse_sort(args: dict[str, Any]) -> list[tuple[str, bool]]:
    """Parse a ``sort`` step into ``[(column, descending), ...]`` keys, in order.

    ``by`` is a column name / ``ur.col(name)`` or a list of them; ``descending`` is
    one bool for every key or a list with one bool per key."""
    by = args["by"]
    items = list(by) if isinstance(by, (list, tuple)) else [by]
    if not items:
        raise ValueError("sort() needs at least one column (got an empty list).")
    names: list[str] = []
    for k in items:
        if isinstance(k, str):
            names.append(k)
        elif getattr(k, "kind", None) == "col":
            names.append(k.payload["name"])
        else:
            raise ValueError(f"sort() keys must be column names or ur.col(<name>); got {k!r}.")
    descending = args.get("descending", False)
    if isinstance(descending, (list, tuple)):
        if len(descending) != len(names):
            raise ValueError(
                f"sort() got {len(descending)} descending flag(s) for {len(names)} "
                f"column(s); pass one bool, or one per column."
            )
        flags = [bool(d) for d in descending]
    else:
        flags = [bool(descending)] * len(names)
    return list(zip(names, flags, strict=True))


# --- the pyarrow tail -------------------------------------------------------
# Relational steps take effect in the order they are written, as in Polars (#162,
# #164): `.select("c").distinct()` dedups the selected column, `.rename({"v":
# "val"}).sort("val")` sorts by the new name, `.head(3).sort("v")` sorts three rows.
# The executor runs the leading steps its engine tail runs identically and hands
# the rest here (`_Host.apply_steps`), as does every step on a plain source. Each
# is parsed into a `(kind, arg)` step and applied in pyarrow, in order:
#   ("filter", Expr)  ("sort", <_parse_sort>)  ("head", n)  ("distinct", None)
#   ("sample", (n, seed))  ("rename", {old: new})  ("select", [names])
#   ("group_by", <_parse_group_agg>)
_Step = tuple[str, Any]


def _tail_step(step: _PlanStep) -> _Step:
    """Parse one relational plan step into a tail step."""
    op, args = step.op, step.args
    if op == "filter":
        return ("filter", args["predicate"])
    if op == "sort":
        return ("sort", _parse_sort(args))
    if op == "head":
        return ("head", int(args["n"]))
    if op == "distinct":
        return ("distinct", None)
    if op == "sample":
        return ("sample", (int(args["n"]), args.get("seed")))
    if op == "rename":
        return ("rename", dict(args["mapping"]))
    if op == "select":
        return ("select", _parse_select(args["columns"]))
    if op == "group_by_agg":
        return ("group_by", _parse_group_agg(step))
    raise AssertionError(f"not a relational tail step: {op!r}")


def _apply_pyarrow_sort(table: Any, sort: list[tuple[str, bool]]) -> Any:
    for by, _ in sort:
        if by not in table.column_names:
            raise ValueError(f"sort references unknown column '{by}'.")
    return _sort_pyarrow(table, sort)


def _apply_pyarrow_steps(table: Any, steps: list[_Step], roles: dict[str, str]) -> Any:
    """Apply tail ``steps`` to a pyarrow table, each in the order written. ``roles``
    resolves ur.id()/src()/dst() in a filter to real column names."""
    for kind, arg in steps:
        if kind == "filter":
            _require_predicate(arg)  # same top-of-predicate check as the graph path
            table = table.filter(_eval_pyarrow(arg, table, roles))
        elif kind == "group_by":
            table = _apply_pyarrow_group_by(table, arg[0], arg[1])
        elif kind == "distinct":
            # Distinct rows over all columns (group-by with no aggregations), each
            # at its first occurrence.
            names = table.column_names
            table = _group_in_first_order(table, names, []).select(names)
        elif kind == "sample":
            table = _sample_pyarrow(table, arg[0], arg[1])
        elif kind == "sort":
            table = _apply_pyarrow_sort(table, arg)
        elif kind == "head":
            table = table.slice(0, arg)
        elif kind == "rename":
            table = _apply_pyarrow_rename(table, arg)
        elif kind == "select":
            table = _select_table(table, arg)
        else:
            raise AssertionError(f"unknown tail step {kind!r}")
    return table


# --- join ------------------------------------------------------------------
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


# --- plain-source collection (no algorithm/traversal) ----------------------
# Materialize a frame's own rows and apply the filter/sort/head tail in pyarrow.
# Used by read_edges/read_nodes and scan_*/from_* frames collected without a
# graph op (e.g. scan_edges(...).collect()).
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
    #
    # pyarrow's hash sum/mean/min/max have no dictionary kernels, so a dictionary-
    # encoded (categorical) operand is aggregated through a decoded copy under a
    # private name. Group *keys* stay as-is: hash grouping handles dictionaries and
    # the key column keeps the user's type in the output.
    import pyarrow as pa

    operand: dict[str, str] = {}
    for a in aggs:
        col = a["column"]
        if col in operand:
            continue
        operand[col] = col
        if pa.types.is_dictionary(table.schema.field(col).type):
            tmp = f"__ursa_decoded_{col}"
            while tmp in table.column_names:
                tmp += "_"
            operand[col] = tmp
            table = table.append_column(tmp, _decode_dictionary(table.column(col)))
    specs = []
    seen_specs: set[tuple[str, str]] = set()
    for a in aggs:
        spec = (operand[a["column"]], _PC_AGG[a["fn"]])
        if spec not in seen_specs:
            seen_specs.add(spec)
            specs.append(spec)
    grouped = _group_in_first_order(table, keys, specs)
    cols = {name: grouped.column(name) for name in grouped.column_names}

    ordered_names = list(keys)
    ordered_cols = [cols[k] for k in keys]
    for a in aggs:
        ordered_names.append(a["output"])
        ordered_cols.append(cols[f"{operand[a['column']]}_{_PC_AGG[a['fn']]}"])
    return pa.table(dict(zip(ordered_names, ordered_cols, strict=True)))


def _group_in_first_order(table: Any, keys: list[str], specs: list[Any]) -> Any:
    """``table.group_by(keys).aggregate(specs)`` with one row per group in order of
    the group's first input row, the order the engine path gives (#168).

    pyarrow's hash grouping guarantees no output order (not even single-threaded),
    so each row is numbered, each group keeps its minimum number, and the groups
    are put in that order."""
    import pyarrow as pa
    import pyarrow.compute as pc

    row = "__ursa_row"
    while row in table.column_names:
        row += "_"
    ones = pa.nulls(table.num_rows, pa.int64()).fill_null(1)
    numbered = table.combine_chunks().append_column(row, pc.cumulative_sum(ones))  # ty: ignore[unresolved-attribute]
    grouped = numbered.group_by(keys).aggregate([*specs, (row, "min")])
    first = f"{row}_min"
    grouped = grouped.take(pc.sort_indices(grouped.column(first)))  # ty: ignore[unresolved-attribute]
    return grouped.drop_columns([first])


def _decode_dictionary(column: Any) -> Any:
    """A dictionary-encoded (categorical) column decoded to its value type; any
    other column is returned unchanged."""
    import pyarrow as pa

    if pa.types.is_dictionary(column.type):
        return column.cast(column.type.value_type)
    return column


def _sort_pyarrow(table: Any, keys: list[tuple[str, bool]]) -> Any:
    """Stable-sort ``table`` lexicographically by ``keys`` — ``(column,
    descending)`` pairs, later keys breaking ties in earlier ones — with nulls last
    for every key. pyarrow has no sort kernel for dictionary arrays, so each sort
    *key* is decoded — ordering by the values (lexicographic for strings), never
    the dictionary indices — while the output keeps the original (dictionary)
    columns."""
    import pyarrow as pa
    import pyarrow.compute as pc

    key_table = pa.table(
        {f"k{i}": _decode_dictionary(table.column(by)) for i, (by, _) in enumerate(keys)}
    )
    sort_keys = [
        (f"k{i}", "descending" if descending else "ascending")
        for i, (_, descending) in enumerate(keys)
    ]
    return table.take(pc.sort_indices(key_table, sort_keys=sort_keys))  # ty: ignore[unresolved-attribute]


def _edge_source_table(frame: EdgeFrame, columns: list[str] | None = None) -> Any:
    """The raw edge rows of an edge frame as a pyarrow.Table — the full in-memory
    edge table, or a fresh scan of its ``scan_edges`` source. For a traversal-result
    frame (#116) this resolves to the *parent* edge rows (the CSR the traversal masks;
    its own ``src``/``dst`` are the synthetic reached-pair columns). Raises the precise
    plan-aware error for a frame with no materializable source.

    A scan reads every column of the file, named, ordered and typed as the same file
    read into memory and passed to ``EdgeFrame`` (string_view read as string), so a
    scanned frame collects exactly what an in-memory one does (#160). ``columns`` is
    a scan projection pushdown: when given, only those columns (plus the endpoints)
    are read, in file order; ``None`` reads all of them. It is ignored for an
    in-memory table (already resident)."""
    import pyarrow as pa

    frame = _parent_edges(frame)  # traversal -> the parent rows the topology is built from
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
        batches = _native().scan_edges_table_arrow(
            path, scan["src"], scan["dst"], _scan_storage_options(scan), columns or []
        )
        if scan.get("on_null") == "drop":
            batches = _drop_null_endpoint_batches(batches, (scan["src"], scan["dst"]))
        return pa.Table.from_batches(batches)
    # No source: a filtered/derived (or traversal-result) edge frame.
    _require_edges(frame)  # raises the precise, plan-aware message
    raise AssertionError("unreachable")  # _require_edges always raises here


def _edge_mask_for(edges: EdgeFrame | None, index: Any) -> Any | None:
    """The subgraph-view edge mask for a derived edge frame, or ``None`` (full graph).

    The parent topology is kept; the surviving rows are expressed as a boolean pyarrow
    array aligned with the CSR's ``edge_ids``. Two sources of mask, combined by AND:

    * ``filter`` (#114): each predicate is evaluated over the *parent* edge rows (the
      same rows, in the same order, the topology was built from — via
      ``_edge_source_table``); repeated ``.filter()`` calls intersect. Endpoint
      predicates (``ur.src()``/``ur.dst()``) and edge-attribute columns are supported;
      a scan reads only the endpoints and the columns the predicates reference.
    * ``hop``/``shortest_path`` (#116): the traversal produces a reached node *set*
      (run over ``index``, the parent CSR); the mask keeps an edge iff **both**
      endpoints are reached — the induced subgraph of the reached region.

    Returns ``None`` when the frame carries neither (the full graph).

    Frames are immutable, so the mask is memoized on the frame's own
    ``_mask_build_cell`` (#175) and later collects skip the source re-read and
    predicate / traversal evaluation. It is stored alongside the ``index`` it was
    computed against and only served for that same index object, so a rebuilt parent
    CSR (e.g. a weighted scan re-scans and replaces it) recomputes the mask.
    """
    if edges is None:
        return None
    traversal = _traversal_step(edges)
    predicates = [s.args["predicate"] for s in getattr(edges, "_plan", ()) if s.op == "filter"]
    if traversal is None and not predicates:
        return None
    cell = edges._mask_build_cell
    cached = cell.value
    if cached is not None and cached[0] is index:
        return cached[1]
    with cell.lock:
        cached = cell.value
        if cached is not None and cached[0] is index:
            return cached[1]
        mask = _compute_edge_mask(edges, index, traversal, predicates)
        cell.value = (index, mask)
    return mask


def _compute_edge_mask(
    edges: EdgeFrame, index: Any, traversal: Any | None, predicates: list[Any]
) -> Any:
    """Evaluate the edge mask described in ``_edge_mask_for`` (uncached)."""
    import pyarrow as pa
    import pyarrow.compute as pc

    parent = _parent_edges(edges)
    roles = {"src": parent.src_col, "dst": parent.dst_col}
    needed = {parent.src_col, parent.dst_col}
    for predicate in predicates:
        needed |= _expr_columns(predicate, roles)
    # Parent edge rows in topology row order, projected to what the mask reads.
    table = _edge_source_table(edges, sorted(needed))
    and_ = pc.and_  # ty: ignore[unresolved-attribute]
    mask = None

    if traversal is not None:
        # The reached node set induces the subgraph: keep e iff both endpoints reached.
        reached = _traversal_reached_nodes(edges, traversal, index)
        is_in = pc.is_in  # ty: ignore[unresolved-attribute]
        src_in = is_in(table.column(parent.src_col), value_set=reached)
        dst_in = is_in(table.column(parent.dst_col), value_set=reached)
        mask = and_(src_in, dst_in)

    for predicate in predicates:
        _require_predicate(predicate)  # same top-of-predicate check as every path
        m = _eval_pyarrow(predicate, table, roles)
        mask = m if mask is None else and_(mask, m)
    # The FFI takes a single contiguous boolean Array (not a ChunkedArray).
    if isinstance(mask, pa.ChunkedArray):
        mask = mask.combine_chunks()
    return mask


def _traversal_reached_nodes(edges: EdgeFrame, traversal: Any, index: Any) -> Any:
    """The reached node set (as a pyarrow array of user ids) behind a graph op over a
    traversal result (#116) — the seeds' k-hop region, or the shortest path's nodes,
    computed over ``index`` (the parent CSR). Mirrors the executor's
    traversal execution so the mask agrees with what the traversal itself would emit."""
    if traversal.op == "hop":
        seeds = _resolve_seeds(traversal.args["seeds"])
        return _native().hop_reached_nodes_query(
            index, seeds, int(traversal.args["n"]), traversal.args["direction"]
        )
    # shortest_path: a weight= expression (over parent edge columns) selects weighted
    # Dijkstra, serialized and evaluated in Rust against the parent edge attribute batch.
    weight = traversal.args.get("weight")
    weight_json = None
    edge_attr = None
    if weight is not None:
        weight_json = json.dumps(_expr_to_json(weight))
        edge_attr = _prepare_weighted(_parent_edges(edges), _expr_columns(weight))
    return _native().shortest_path_nodes_query(
        index,
        _resolve_seeds([traversal.args["source"]]),
        _resolve_seeds([traversal.args["target"]]),
        traversal.args["direction"],
        weight_json,
        edge_attr,
    )


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
