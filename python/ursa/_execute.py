"""Executing a frame — Ursa's ``collect()``.

``collect()`` serializes the frame's plan to the Rust plan IR
(``ursa_plan::plan::Plan``) and hands it to the one native entry point,
``ursa._ursa.run_plan``. Rust decides how the plan runs: it walks the tree, rejects
the shapes that aren't supported, and maps the rest onto its executors.

Scans, subgraph masks, weights and every ``filter`` run in Rust (#192). What stays
here is what belongs to the Python frames. ``run_plan`` reads it back through
``_Host``, by slot, as it needs it:

- each frame's source: its in-memory table, or its scan's storage options;
- the per-frame cells (``_frames._BuildCell``): the topology index (which carries
  the weight and mask caches), and a scanned node table's native cache;
- seed id sets;
- the pyarrow tail: the relational steps other than ``filter`` that the engine's
  fixed-order tail can't run as written (#191 retires it).
"""

from __future__ import annotations

import json
import math
from typing import TYPE_CHECKING, Any

from ._result import MaterializedFrame

if TYPE_CHECKING:
    from ._frames import EdgeFrame, _Frame, _PlanStep

# The lazy index build is guarded per-frame by the lock on each frame's shared
# `_BuildCell` (see `_frames._BuildCell`), so first-collects of *unrelated* frames
# from different threads build concurrently instead of serializing on one global
# lock. collect() runs GIL-free, so the memo assignment itself still needs the
# cell's lock.


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

    __slots__ = ("_errors", "_frame_slots", "_slots", "_steps")

    def __init__(self) -> None:
        self._slots: list[Any] = []
        self._frame_slots: dict[int, int] = {}
        self._steps: list[_PlanStep] = []
        # step handle -> the error parsing that step's arguments raised. It is
        # raised when the executor reaches the step (`step_error`), so a shape
        # guard that fires first still wins, as when steps were parsed lazily.
        self._errors: dict[int, BaseException] = {}

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

    def _defer(self, step: _PlanStep, error: BaseException) -> int:
        handle = self._handle(step)
        self._errors[handle] = error
        return handle

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
                "n": _int_json(a["n"]),
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
                "steps": _int_json(a["steps"]),
                "walks_per_node": _int_json(a["walks_per_node"]),
                "seed": _int_json(a.get("seed")),
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
            except Exception as exc:  # raised where the right side runs
                right = {"op": "invalid", "step": self._defer(step, exc)}
            node = {"op": op, "input": input, "right": right, "how": a["how"]}
            try:
                node.update(on=_parse_join_keys(a["on"]), step=self._handle(step))
            except Exception as exc:
                node.update(invalid=True, step=self._defer(step, exc))
            return node
        try:
            args = self._tail_args(op, step)
        except Exception as exc:
            return {"op": op, "input": input, "step": self._defer(step, exc), "invalid": True}
        return {"op": op, "input": input, "step": self._handle(step), **args}

    def _tail_args(self, op: str, step: _PlanStep) -> dict[str, Any]:
        """A relational step's arguments, parsed. An unknown op fails the plan's
        parse."""
        a = step.args
        if op == "filter":
            return {"predicate": _expr_to_json(a["predicate"])}
        if op == "select":
            return {"columns": _parse_select(a["columns"])}
        if op == "rename":
            return {"mapping": [[old, new] for old, new in dict(a["mapping"]).items()]}
        if op == "sort":
            return {"by": [{"column": c, "descending": d} for c, d in _parse_sort(a)]}
        if op == "head":
            return {"n": int(a["n"])}
        if op == "sample":
            return {"n": int(a["n"]), "seed": _int_json(a.get("seed"))}
        if op == "group_by_agg":
            keys, aggs = _parse_group_agg(step)
            return {"keys": keys, "aggs": aggs}
        return {}  # distinct

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

    def built_index(self, slot: int) -> Any | None:
        return self._slots[slot]._index_build_cell.value

    def put_index(self, slot: int, index: Any) -> Any:
        """Keep an index built from a weighted scan's rows, unless another collect
        built one meanwhile; the frame's index either way."""
        cell = self._slots[slot]._index_build_cell
        with cell.lock:
            if cell.value is None:
                cell.value = index
            return cell.value

    def table(self, slot: int) -> Any | None:
        """An in-memory frame's table: a node frame's attribute table, or an edge
        frame's whole edge table (``None`` without one)."""
        from ._frames import NodeFrame

        frame = self._slots[slot]
        if isinstance(frame, NodeFrame):
            return frame._attr_table
        return frame._edge_attr_table

    def storage_options(self, slot: int) -> dict[str, Any] | None:
        return _scan_storage_options(self._slots[slot]._scan_spec)

    def node_cache(self, slot: int) -> Any:
        """A ``scan_nodes`` frame's native table cache, kept in its shared cell."""
        cell = self._slots[slot]._attr_build_cell
        if cell.value is None:
            with cell.lock:
                if cell.value is None:
                    cell.value = _native().NodeTableCache()
        return cell.value

    def warn_dropped(self, n: int) -> None:
        from ._io import _warn_dropped_endpoints

        _warn_dropped_endpoints(n)

    def seeds(self, slot: int) -> Any:
        return _resolve_seeds(self._slots[slot])

    def apply_steps(self, rows: Any, handles: list[int], roles: dict[str, str]) -> Any:
        """Run the steps behind ``handles`` (never a ``filter``: those run in Rust)
        in pyarrow, on a table this host produced or on the engine's batch list."""
        import pyarrow as pa

        table = pa.Table.from_batches(rows) if isinstance(rows, list) else rows
        steps = [_tail_step(self._steps[h]) for h in handles]
        return _apply_pyarrow_steps(table, steps, roles)

    def batches(self, table: Any) -> list[Any]:
        return _batches(table)

    def step_error(self, handle: int) -> None:
        raise self._errors[handle]


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
        return _lit_json(p["value"])
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


def _lit_json(value: Any) -> dict[str, Any]:
    """A literal node. A plain or numpy bool, int, float or string, or None, is its
    JSON value. A value JSON can't hold takes the type ``pyarrow.scalar`` gives it,
    so it compares as it always did (``ursa_plan::expr::parse_typed_literal``):

    - NaN and infinity: ``{"type": "float64", "value": "nan" | "inf" | "-inf"}``;
    - a datetime / ``np.datetime64``: ``timestamp`` (``unit``, ``tz``, an integer);
    - a date: ``date32`` (days); a time: ``time`` (``unit``, an integer);
    - a timedelta / ``np.timedelta64``: ``duration`` (``unit``, an integer);
    - a ``Decimal``: ``decimal`` (``precision``, ``scale``, the unscaled digits).

    Anything else is marked ``unrepresentable`` and rejected with the
    ``TypeError`` JSON encoding raised for it."""
    dtype = getattr(value, "dtype", None)
    if type(value).__module__ == "numpy" and getattr(dtype, "kind", "") in "biuf":
        value = value.item()  # a numpy bool/int/float scalar -> its Python value
    if value is None or isinstance(value, (bool, str, int)):
        return {"kind": "lit", "value": value}
    if isinstance(value, float):
        if math.isfinite(value):
            return {"kind": "lit", "value": value}
        text = "nan" if math.isnan(value) else ("inf" if value > 0 else "-inf")
        return {"kind": "lit", "type": "float64", "value": text}
    typed = _typed_lit_json(value)
    if typed is not None:
        return typed
    return {"kind": "lit", "value": None, "unrepresentable": type(value).__name__}


def _typed_lit_json(value: Any) -> dict[str, Any] | None:
    """The typed literal for ``value`` (see ``_lit_json``), or None."""
    import pyarrow as pa
    import pyarrow.types as pat

    try:
        scalar = pa.scalar(value)
    except Exception:  # a value pyarrow has no scalar for
        return None
    t = scalar.type
    if pat.is_timestamp(t):
        return {
            "kind": "lit",
            "type": "timestamp",
            "unit": t.unit,
            "tz": t.tz,
            "value": scalar.value,
        }
    if pat.is_date32(t):
        return {"kind": "lit", "type": "date32", "value": scalar.value}
    if pat.is_time(t):
        return {"kind": "lit", "type": "time", "unit": t.unit, "value": scalar.value}
    if pat.is_duration(t):
        return {"kind": "lit", "type": "duration", "unit": t.unit, "value": scalar.value}
    if pat.is_decimal128(t):
        digits = int(scalar.as_py().scaleb(t.scale))
        return {
            "kind": "lit",
            "type": "decimal",
            "precision": t.precision,
            "scale": t.scale,
            "value": str(digits),
        }
    return None


def _int_json(value: Any) -> Any:
    """An integer argument as given, for ``IntArg``: the engine converts it where
    it reads it, so the host's evaluator keeps accepting what it accepted."""
    if value is None or isinstance(value, (bool, int, str)):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else {"type": "float"}
    if hasattr(value, "__index__"):
        return value.__index__()
    return {"type": type(value).__name__}


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


def _scan_storage_options(scan: dict[str, Any]) -> dict[str, Any] | None:
    """The storage_options for a scan spec, after rejecting the unsupported
    ``store=`` (obstore) parameter. ``s3://``/``gs://``/``az://`` credentials and
    config flow through storage_options; obstore interop is future work."""
    if scan.get("store") is not None:
        raise NotImplementedError(
            "store= (a pre-configured obstore store) is not supported yet; pass "
            "storage_options={...} instead."
        )
    return scan.get("storage_options")


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
        # A plain attribute table yields its ids directly. A plain scan reads just
        # the id column (through the frame's node cache: a 50-column node file
        # used only for seeds reads one column); a derived frame (filtered /
        # computed) must be executed to get them.
        if plain and seeds._attr_table is not None:
            tbl = seeds._attr_table  # a (chunked) pyarrow.Table
        elif plain and seeds._scan_spec is not None:
            tbl = seeds.select(seeds.id_col).collect().to_arrow()
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
        # A single-path scan builds natively (`_scan_build_index`); a list of
        # paths is not supported yet.
        raise NotImplementedError(
            "collect() over a scan_edges source supports a single string path "
            "(glob included) for now, not a list of paths."
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
    # (The executor checks the edge plan before it asks for the index, and asks a
    # traversal result's input frame for it: see ursa_plan::plan::require_index.)
    cell = edges._index_build_cell
    idx = cell.value
    if idx is not None:
        return idx
    with cell.lock:
        idx = cell.value
        if idx is None:
            idx = _scan_build_index(edges)
            if idx is None:
                idx = _native().build_index(_require_edges(edges))
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
# the rest here (`_Host.apply_steps`), as it does the steps on a plain source,
# except `filter`, which the executor evaluates itself. Each is parsed into a
# `(kind, arg)` step and applied in pyarrow, in order:
#   ("sort", <_parse_sort>)  ("head", n)  ("distinct", None)
#   ("sample", (n, seed))  ("rename", {old: new})  ("select", [names])
#   ("group_by", <_parse_group_agg>)
_Step = tuple[str, Any]


def _tail_step(step: _PlanStep) -> _Step:
    """Parse one relational plan step into a tail step."""
    op, args = step.op, step.args
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
    would resolve role references; no step here takes one (filters run in Rust)."""
    del roles
    for kind, arg in steps:
        if kind == "group_by":
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


# --- the rest of the pyarrow tail (#191) ---------------------------------------
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


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
