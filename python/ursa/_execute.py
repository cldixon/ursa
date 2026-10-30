"""The host side of a frame's plan: what the Rust plan object calls back into.

A frame holds a Rust plan object (``ursa._ursa.PlanNode``), which plans,
validates and runs it. Rust reads the frame's own Python objects (its tables and
endpoint arrays) directly; these few callbacks are what stays in Python:

- the ``(src, dst)`` batches an in-memory edge frame's index is built from;
- a seed set given as an iterable of ids, converted to an Arrow array;
- the rows a ``sample`` keeps, drawn with the stdlib's seeded generator, so any
  seed Python's ``random`` accepts keeps working;
- the ``on_null="drop"`` warning, and an in-memory table's identity token.
"""

from __future__ import annotations

import itertools
import threading
import weakref
from typing import Any

# A sample with no seed draws with this one, so it is reproducible too.
_DEFAULT_SAMPLE_SEED = 0x5EED5EED


def _edge_batches(endpoints: tuple[Any, Any], reversed: bool) -> list[Any]:
    """Zero-copy ``[src, dst]`` batches over a frame's canonical endpoint arrays
    (chunk boundaries that differ between the two columns are sliced, not
    copied), for the index build; swapped for a reversed frame."""
    import pyarrow as pa

    src, dst = endpoints
    if reversed:
        src, dst = dst, src
    table = pa.Table.from_arrays([src, dst], names=["src", "dst"])
    return table.to_batches() or [
        pa.RecordBatch.from_arrays([col.combine_chunks() for col in table.columns], ["src", "dst"])
    ]


def _seeds(seeds: Any) -> Any:
    """A seed set given as an iterable of node ids, as an int64 or string array."""
    import pyarrow as pa

    if seeds is None:
        raise NotImplementedError(
            "a seed set is required: pass an iterable of node ids or a NodeFrame "
            "(ur.hop(...).from_(ids); ur.random_walk(..., start=ids))."
        )
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


def _sample(draw: tuple[int, Any], rows: int) -> list[int] | None:
    """The positions (ascending) a ``sample(n, seed=)`` keeps out of ``rows``
    rows, or None for all of them: a seeded stdlib draw over the row positions."""
    import random

    n, seed = draw
    if n >= rows:
        return None
    rng = random.Random(seed if seed is not None else _DEFAULT_SAMPLE_SEED)
    return sorted(rng.sample(range(rows), n))


def _warn_dropped(n: int) -> None:
    from ._io import _warn_dropped_endpoints

    _warn_dropped_endpoints(n)


# One token per in-memory table object while it lives: the identity of a
# reshaped edge set's input in the native topology cache's key (#191). A token is
# never reused, so a new table can't be mistaken for one that was freed.
_TABLE_TOKENS: dict[int, str] = {}
_TOKEN_LOCK = threading.Lock()
_next_token = itertools.count(1).__next__


def _table_token(table: Any) -> str | None:
    key = id(table)
    with _TOKEN_LOCK:
        token = _TABLE_TOKENS.get(key)
        if token is None:
            token = _TABLE_TOKENS[key] = str(_next_token())
            weakref.finalize(table, _TABLE_TOKENS.pop, key, None)
    return token


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
