"""Python input to the plan IR: what a frame method converts before it calls the
Rust plan object.

Expressions become the JSON grammar of ``ursa_plan::expr`` (as Python dicts),
and step arguments the shapes the plan's steps take. Which steps, verbs,
aggregations and joins exist, and whether a step fits the frame it is added to,
is the Rust plan's to decide: nothing here enumerates them. A conversion that
fails is not raised here: ``capture`` hands the error to the plan object, which
raises it where a collect would have.
"""

from __future__ import annotations

import math
from collections.abc import Callable
from typing import Any


def capture(convert: Callable[..., Any], *args: Any) -> tuple[Any, BaseException | None]:
    """``(convert(*args), None)``, or ``(None, error)`` if it raised."""
    try:
        return convert(*args), None
    except Exception as exc:
        return None, exc


# --- expressions -------------------------------------------------------------
def expr(e: Any, where: str = "this argument") -> dict[str, Any]:
    """Serialize an ``Expr`` tree to the JSON grammar of ``ursa_plan::expr``:
    ``col`` / ``lit`` / ``binary`` / ``unary`` / ``agg`` / ``alias``, and the role
    references ``src`` / ``dst`` / ``id``, which the executor resolves to columns
    for the context the expression runs in. A value that is no expression raises
    the ``AttributeError`` reading it gave, naming ``where`` it was passed."""
    if not (hasattr(e, "kind") and hasattr(e, "payload")):
        raise AttributeError(
            f"{where} takes an ursa expression, such as ur.col('x') or ur.col('x') > 1; "
            f"got {e!r} ({type(e).__name__})"
        )
    kind, p = e.kind, e.payload
    if kind == "col":
        return {"kind": "col", "name": p["name"]}
    if kind == "lit":
        return _lit(p["value"])
    if kind == "binary":
        return {"kind": "binary", "op": p["op"], "left": expr(p["left"]), "right": expr(p["right"])}
    if kind == "unary":
        return {"kind": "unary", "op": p["op"], "operand": expr(p["operand"])}
    if kind == "agg":
        return {"kind": "agg", "fn": p["fn"], "operand": expr(p["operand"])}
    if kind == "alias":
        return {"kind": "alias", "name": p["name"], "operand": expr(p["operand"])}
    return {"kind": kind}


def optional_expr(e: Any, where: str = "this argument") -> dict[str, Any] | None:
    return None if e is None else expr(e, where)


def column(e: Any) -> tuple[dict[str, Any], Any]:
    """A ``with_columns`` expression: a graph verb's parameters and the plan of
    the edge frame it runs over, or any other expression (with no edges; the
    engine rejects a value that is no expression with its own error)."""
    if getattr(e, "kind", None) != "graph":
        return (expr(e) if hasattr(e, "kind") else {"kind": "unknown"}), None
    out: dict[str, Any] = {"kind": "graph"}
    edges = None
    call = _verb_call(e.payload.get("verb"))
    for key, value in e.payload.items():
        if key == "edges":
            edges = None if value is None else edges_node(value, call)
        elif key in ("weight", "agg"):
            out[key] = optional_expr(value, f"{call}({key}=)")
        elif key == "from_":
            out[key] = value is not None
        else:
            out[key] = value
    return out, edges


def _verb_call(verb: Any) -> str:
    """The call a graph verb is written as (``pagerank``, ``neighbors().agg``)."""
    return "neighbors().agg" if verb == "neighbors_agg" else str(verb)


def edges_node(edges: Any, call: str) -> Any:
    """The plan of the edge frame a graph verb or traversal ``call`` runs over;
    for a value that is no frame, the ``AttributeError`` reading its plan gave,
    naming the call."""
    node = getattr(edges, "_node", None)
    if node is None:
        raise AttributeError(
            f"{call}() takes an EdgeFrame (the graph) as its edges; got {type(edges).__name__}"
        )
    return node


def count(n: Any, call: str) -> int:
    """A row count argument (``head(n)``, ``sample(n)``) as ``int(n)`` reads it,
    with the exception ``int`` raised naming the call."""
    try:
        return int(n)
    except (TypeError, ValueError) as exc:
        raise type(exc)(
            f"{call} takes an integer row count; got {n!r} ({type(n).__name__})"
        ) from None


def _lit(value: Any) -> dict[str, Any]:
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
    typed = _typed_lit(value)
    if typed is not None:
        return typed
    return {"kind": "lit", "value": None, "unrepresentable": type(value).__name__}


def _typed_lit(value: Any) -> dict[str, Any] | None:
    """The typed literal for ``value`` (see ``_lit``), or None."""
    import decimal

    import pyarrow as pa
    import pyarrow.types as pat

    if type(value).__module__ == "numpy" and getattr(value.dtype, "kind", "") in "mM":
        value = _numpy_temporal(value)
        if isinstance(value, dict):
            return value  # a unit no Arrow type holds: rejected
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
    if pat.is_decimal(t):
        # The unscaled digits, exactly: the default context keeps only 28.
        with decimal.localcontext(decimal.Context(prec=t.precision + 2)):
            digits = int(scalar.as_py().scaleb(t.scale))
        return {
            "kind": "lit",
            "type": "decimal",
            "precision": t.precision,
            "scale": t.scale,
            "value": str(digits),
        }
    if pat.is_binary(t):
        return {"kind": "lit", "type": "binary", "value": scalar.as_py().hex()}
    # Anything else pyarrow makes (a list, a struct, ...) travels as Arrow IPC.
    sink = pa.BufferOutputStream()
    batch = pa.record_batch([pa.array([scalar.as_py()], t)], names=["v"])
    with pa.ipc.new_stream(sink, batch.schema) as writer:
        writer.write_batch(batch)
    return {"kind": "lit", "type": "arrow", "value": sink.getvalue().to_pybytes().hex()}


def _numpy_temporal(value: Any) -> Any:
    """A numpy datetime64/timedelta64 in a unit pyarrow takes: a day-based
    datetime64 as a date, a week/day/hour/minute timedelta64 in seconds. A
    month or year unit has no Arrow type: the unrepresentable marker for it."""
    import numpy as np

    unit = np.datetime_data(value.dtype)[0]
    if value.dtype.kind == "M":
        if unit in ("D", "W"):
            return value.astype("datetime64[D]").item()
    elif unit in ("W", "D", "h", "m"):
        return value.astype("timedelta64[s]")
    if unit in ("M", "Y"):
        name = type(value).__name__
        return {
            "kind": "lit",
            "value": None,
            "unrepresentable": name,
            "reason": f"a numpy {name} with unit '{unit}' has no fixed length; "
            "use a unit of a day or finer in a filter",
        }
    return value


def int_arg(value: Any) -> Any:
    """An integer argument as given, for ``IntArg``: the engine converts it where
    it reads it, so it keeps accepting what Python accepts."""
    if value is None or isinstance(value, (bool, int, str)):
        return value
    if isinstance(value, float):
        return value if math.isfinite(value) else {"type": "float"}
    if hasattr(value, "__index__"):
        return value.__index__()
    return {"type": type(value).__name__}


# --- step arguments ----------------------------------------------------------
def select_items(columns: Any) -> list[Any]:
    """The ``select(...)`` items: a column name (a bare name or ``ur.col(name)``),
    or any other expression, serialized (``ursa_plan::plan::SelectItem``)."""
    out: list[Any] = []
    for c in columns:
        if isinstance(c, str):
            out.append(c)
            continue
        kind = getattr(c, "kind", None)
        if kind == "col":
            out.append(c.payload["name"])
        elif kind is not None and kind != "graph":
            out.append(expr(c))
        else:
            raise NotImplementedError(
                "select() takes column names, ur.col(<name>) or expressions over columns "
                "(e.g. .select('id', (ur.col('w') * 2).alias('w2')))."
            )
    return out


def rename_mapping(mapping: Any) -> list[list[str]]:
    """The ``rename({old: new})`` pairs. A mapping ``dict()`` can't read, or a name
    that is no string, raises the exception it always did (``dict()``'s, or the
    plan's ``ValueError``/``TypeError``), worded for ``rename()``."""
    try:
        pairs = dict(mapping).items()
    except (TypeError, ValueError) as exc:
        raise type(exc)(
            f"rename() takes a dict of {{old: new}} column names; got {mapping!r} "
            f"({type(mapping).__name__})"
        ) from None
    for name in (n for pair in pairs for n in pair):
        if not isinstance(name, str):
            json_like = name is None or isinstance(name, (bool, int, float, list, tuple, dict))
            raise (ValueError if json_like or hasattr(name, "__index__") else TypeError)(
                f"rename() maps column names to new names, both strings; got {name!r} "
                f"({type(name).__name__})"
            )
    return [[old, new] for old, new in pairs]


def sort_keys(by: Any, descending: Any) -> list[dict[str, Any]]:
    """A ``sort``'s keys, in order: each a column name or an expression, with its
    direction. ``by`` is a column name / ``ur.col(name)`` / expression or a list
    of them; ``descending`` is one bool for every key or a list with one bool per
    key."""
    items = list(by) if isinstance(by, (list, tuple)) else [by]
    if not items:
        raise ValueError("sort() needs at least one column (got an empty list).")
    keys: list[Any] = []
    for k in items:
        if isinstance(k, str):
            keys.append(k)
        elif getattr(k, "kind", None) == "col":
            keys.append(k.payload["name"])
        elif getattr(k, "kind", None) not in (None, "graph"):
            keys.append(k)  # an expression key
        else:
            raise ValueError(
                f"sort() keys must be column names, ur.col(<name>) or expressions; got {k!r}."
            )
    if isinstance(descending, (list, tuple)):
        if len(descending) != len(keys):
            raise ValueError(
                f"sort() got {len(descending)} descending flag(s) for {len(keys)} "
                f"column(s); pass one bool, or one per column."
            )
        flags = [bool(d) for d in descending]
    else:
        flags = [bool(descending)] * len(keys)
    return [
        {"column": k, "descending": d} if isinstance(k, str) else {"expr": expr(k), "descending": d}
        for k, d in zip(keys, flags, strict=True)
    ]


def group_agg(keys: Any, exprs: Any, named: Any) -> tuple[list[str], list[dict[str, str]]]:
    """A ``group_by(keys).agg(*exprs, **named)``: the key names, and one
    ``{output, fn, column}`` per aggregation."""
    from . import _ursa

    out_keys: list[str] = []
    for k in keys:
        if isinstance(k, str):
            out_keys.append(k)
        elif getattr(k, "kind", None) == "col":
            out_keys.append(k.payload["name"])
        else:
            raise NotImplementedError("group_by() supports column names or ur.col(<name>) as keys.")
    if not out_keys:
        raise NotImplementedError("group_by() needs at least one key column.")
    resolved: list[tuple[str, str, str]] = []
    pairs = [(None, e) for e in exprs] + list(named.items())
    for name, e in pairs:
        alias_name: str | None = None
        node = e
        if getattr(node, "kind", None) == "alias":
            alias_name = node.payload["name"]
            node = node.payload["operand"]
        if getattr(node, "kind", None) != "agg":
            raise NotImplementedError(
                "agg() expects an aggregation like ur.col('x').mean() "
                "(fns: mean/sum/min/max/count/n_unique)."
            )
        fn = node.payload["fn"]
        _ursa.check_agg_fn(fn)
        operand = node.payload["operand"]
        if getattr(operand, "kind", None) != "col":
            raise NotImplementedError(
                "agg() supports aggregating a single ur.col(<name>) for now "
                "(aggregating an expression like col('a') * col('b') is future work)."
            )
        col = operand.payload["name"]
        resolved.append((name or alias_name or f"{col}_{fn}", fn, col))
    if not resolved:
        raise NotImplementedError("group_by().agg() needs at least one aggregation.")
    seen: set[str] = set()
    for output, _fn, _col in resolved:
        if output in out_keys:
            raise ValueError(f"aggregation output name {output!r} collides with a group key.")
        if output in seen:
            raise ValueError(f"duplicate aggregation output name {output!r}.")
        seen.add(output)
    return out_keys, [{"output": o, "fn": f, "column": c} for (o, f, c) in resolved]


def join_keys(on: Any) -> list[str]:
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
