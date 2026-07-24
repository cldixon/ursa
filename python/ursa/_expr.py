"""The Ursa expression dialect — deliberately Polars-shaped.

This module is pure Python and always available: it builds an expression *tree*
that the native layer (``ursa-plan::expr``) lowers to a DataFusion expression.
What transfers from Polars is the muscle memory and mental model — not the
import. ``pl.Expr`` objects are **not** accepted, and no translator between the
two dialects will be built (see the spec's Architecture section for why).

    ur.col("amount") * ur.col("fx_rate") > ur.lit(1000)

Graph verbs (``ur.degree``, ``ur.pagerank``, ...) are members of the same
expression family; they live in ``ursa._graph`` and produce ``Expr`` nodes too.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any


@dataclass(frozen=True)
class Expr:
    """A node in the expression tree.

    ``kind`` tags the node ("col", "lit", "src", "binary", "graph", ...); the
    remaining fields carry its payload. This mirrors ``UrsaExpr`` in
    ``ursa-plan`` — the stable, engine-independent representation that crosses the
    FFI and gets lowered to DataFusion exactly once, in one place.
    """

    kind: str
    payload: dict[str, Any] = field(default_factory=dict)

    # -- operator overloads: build binary nodes, Polars-style ----------------
    def _binary(self, op: str, other: Any) -> Expr:
        return Expr("binary", {"op": op, "left": self, "right": _wrap(other)})

    def _rbinary(self, op: str, other: Any) -> Expr:
        return Expr("binary", {"op": op, "left": _wrap(other), "right": self})

    def __add__(self, o: Any) -> Expr:
        return self._binary("+", o)

    def __radd__(self, o: Any) -> Expr:
        return self._rbinary("+", o)

    def __sub__(self, o: Any) -> Expr:
        return self._binary("-", o)

    def __rsub__(self, o: Any) -> Expr:
        return self._rbinary("-", o)

    def __mul__(self, o: Any) -> Expr:
        return self._binary("*", o)

    def __rmul__(self, o: Any) -> Expr:
        return self._rbinary("*", o)

    def __truediv__(self, o: Any) -> Expr:
        return self._binary("/", o)

    def __rtruediv__(self, o: Any) -> Expr:
        return self._rbinary("/", o)

    def __gt__(self, o: Any) -> Expr:
        return self._binary(">", o)

    def __ge__(self, o: Any) -> Expr:
        return self._binary(">=", o)

    def __lt__(self, o: Any) -> Expr:
        return self._binary("<", o)

    def __le__(self, o: Any) -> Expr:
        return self._binary("<=", o)

    # __eq__/__ne__ build comparison expressions (Polars semantics), not bools —
    # a deliberate, documented divergence from object.__eq__.
    def __eq__(self, o: Any) -> Expr:  # ty: ignore[invalid-method-override]
        return self._binary("==", o)

    def __ne__(self, o: Any) -> Expr:  # ty: ignore[invalid-method-override]
        return self._binary("!=", o)

    def __and__(self, o: Any) -> Expr:
        return self._binary("&", o)

    def __or__(self, o: Any) -> Expr:
        return self._binary("|", o)

    def __invert__(self) -> Expr:
        return Expr("unary", {"op": "~", "operand": self})

    __hash__ = None  # type: ignore[assignment]  # Exprs are trees, not dict keys

    # -- aggregations (scoped pragmatically for v0.1) ------------------------
    def _agg(self, name: str) -> Expr:
        return Expr("agg", {"fn": name, "operand": self})

    def mean(self) -> Expr:
        return self._agg("mean")

    def sum(self) -> Expr:
        return self._agg("sum")

    def min(self) -> Expr:
        return self._agg("min")

    def max(self) -> Expr:
        return self._agg("max")

    def count(self) -> Expr:
        return self._agg("count")

    def n_unique(self) -> Expr:
        return self._agg("n_unique")

    def alias(self, name: str) -> Expr:
        return Expr("alias", {"name": name, "operand": self})

    def __repr__(self) -> str:  # readable trees in the REPL and explain()
        k = self.kind
        p = self.payload
        if k == "col":
            return f"col({p['name']!r})"
        if k == "lit":
            return f"lit({p['value']!r})"
        if k in ("src", "dst", "id"):
            return f"{k}()"
        if k == "binary":
            return f"({p['left']!r} {p['op']} {p['right']!r})"
        if k == "unary":
            return f"({p['op']}{p['operand']!r})"
        if k == "agg":
            return f"{p['operand']!r}.{p['fn']}()"
        if k == "alias":
            return f"{p['operand']!r}.alias({p['name']!r})"
        if k == "graph":
            return f"{p['verb']}(...)"
        return f"Expr({k})"


def _wrap(value: Any) -> Expr:
    """Coerce a Python scalar or Expr into an Expr (literals become lit nodes)."""
    return value if isinstance(value, Expr) else lit(value)


# -- constructors ------------------------------------------------------------
def col(name: str) -> Expr:
    """Reference a column by name."""
    return Expr("col", {"name": name})


def lit(value: Any) -> Expr:
    """A scalar literal."""
    return Expr("lit", {"value": value})


def src() -> Expr:
    """The source-role column of the ambient EdgeFrame (abstract reference)."""
    return Expr("src")


def dst() -> Expr:
    """The destination-role column of the ambient EdgeFrame (abstract reference)."""
    return Expr("dst")


def id() -> Expr:
    """The id-role column of the ambient NodeFrame (abstract reference)."""
    return Expr("id")
