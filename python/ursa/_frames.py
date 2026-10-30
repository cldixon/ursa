"""The two public types: ``EdgeFrame`` and ``NodeFrame`` — both lazy.

There is no ``Graph`` object. The EdgeFrame *is* the graph (designated ``src`` /
``dst`` roles plus an internal cached topology index); a NodeFrame is an
attribute table with a designated ``id`` role that relates to it by join
convention. A frame *is* a logical plan until ``.collect()``.

A frame holds one Rust plan object (``ursa._ursa.PlanNode``), as a Polars
``LazyFrame`` does: each method converts its arguments (``_ir``), calls the plan
object, which builds and validates the next step in Rust, and wraps the result
in a new frame of the same type. A step that does not fit the frame (an unknown
column, an unsupported shape) raises on the line that adds it. ``collect()``,
``schema()`` and ``explain()`` run in Rust too, and so do the caches: a frame's
topology index (with its weights, masks and symmetric views) lives on the Rust
state it shares with the derivations that keep its rows.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any, Generic, TypeVar

from . import _ir

if TYPE_CHECKING:
    from collections.abc import Sequence

    # typing.Self is 3.11+; typing_extensions backports it for our 3.10 floor.
    from typing_extensions import Self

    from ._result import MaterializedFrame


def _native() -> Any:
    from ._execute import _native as native

    return native()


class _CacheView:
    """A read-only look at a cache that lives on a frame's Rust plan object."""

    __slots__ = ("value",)

    def __init__(self, value: Any) -> None:
        self.value = value


class _Frame:
    """Shared relational surface for both frame types.

    ``_node`` is the frame's plan: a Rust ``PlanNode``, its last step, holding the
    steps before it.
    """

    __slots__ = ("_node",)
    _node: Any

    # -- internal helpers ----------------------------------------------------
    # Chainable verbs return `Self`, so `EdgeFrame.filter(...)` stays an
    # `EdgeFrame` (and NodeFrame a NodeFrame) through a whole pipeline.
    def _derive(self, node: Any) -> Self:
        new = self.__class__.__new__(self.__class__)
        new._node = node
        return new

    @classmethod
    def _wrap(cls, node: Any) -> Any:
        new = cls.__new__(cls)
        new._node = node
        return new

    # -- relational verbs ----------------------------------------------------
    def filter(self, predicate: Any) -> Self:
        # `filter` is a **subgraph view** on an EdgeFrame (#114): the parent
        # topology is unchanged — the dropped rows are expressed as an edge mask at
        # execution time — so the frame keeps its parent's index. A graph op on a
        # filtered edge frame runs the parent CSR with that mask instead of
        # rebuilding. (distinct/sample/join/group_by start a fresh index: a graph op
        # over them builds one from their rows, cached by the engine under a
        # fingerprint of the edge plan, #191.)
        return self._derive(self._node.filter(*_ir.capture(_ir.expr, predicate)))

    def select(self, *columns: Any) -> Self:
        return self._derive(self._node.select(*_ir.capture(_ir.select_items, columns)))

    def with_columns(self, **exprs: Any) -> Self:
        columns = [(name, *_ir.column(e)) for name, e in exprs.items()]
        return self._derive(self._node.with_columns(columns))

    def rename(self, mapping: dict[str, str]) -> Self:
        return self._derive(self._node.rename(*_ir.capture(_ir.rename_mapping, mapping)))

    def sort(self, by: Any, *more_by: Any, descending: bool | Sequence[bool] = False) -> Self:
        """Sort rows by one or more columns, Polars-style.

        ``by`` is a column name, ``ur.col(name)`` or another expression, or a list of
        them; ``more_by`` appends further keys (``.sort("a", "b")`` is
        ``.sort(["a", "b"])``). Rows are ordered lexicographically by the keys in
        order, so later keys break ties in earlier ones. ``descending`` is a single
        bool applied to every key, or a list with one bool per key. Nulls sort last
        for every key, in either direction. A float key sorts every NaN above every
        number and ``-0.0`` equal to ``0.0``. A dictionary-encoded (categorical) key
        orders by its values, not its codes.
        The sort is stable on every path: rows that tie on every key keep their
        input order, identically on every run. After a graph algorithm that is
        node order, or the attribute table's row order when the frame has one;
        after a traversal it is the traversal's own output order. An empty key
        list, a ``descending`` list whose length differs from the keys, or an
        unknown column raises an error where the sort is added.
        """
        if more_by:
            by = [*(by if isinstance(by, (list, tuple)) else [by]), *more_by]
        return self._derive(self._node.sort(*_ir.capture(_ir.sort_keys, by, descending)))

    def head(self, n: int = 10) -> Self:
        """Keep the first ``n`` rows.

        Without a ``sort`` first, those are the first ``n`` in input order, which
        is fixed on every run and core count: the row order of a plain frame or of
        a frame's attribute table, node order for ``edges.nodes()``, a traversal's
        own output order, and for a ``join`` the left rows in order, each followed
        by its matches in right-frame order.
        """
        return self._derive(self._node.head(*_ir.capture(int, n)))

    limit = head

    def sample(self, n: int, *, seed: int | None = None) -> Self:
        """Keep ``n`` random rows, reproducibly for a ``seed``, in input order.

        The draw is deterministic: ``seed=None`` uses a fixed default seed, so it
        keeps the same rows on every call and every run. Pass a different
        ``seed`` for a different draw."""
        # sample changes the row set, so the frame starts a fresh topology index
        # (like distinct): a later graph op builds one from the sample.
        count, error = _ir.capture(int, n)
        return self._derive(self._node.sample(count, _ir.int_arg(seed), (count, seed), error))

    def distinct(self) -> Self:
        """Remove duplicate rows; each kept row stays at its first occurrence."""
        return self._derive(self._node.distinct())

    def group_by(self, *keys: Any) -> _GroupBy[Self]:
        """Group by ``keys``; ``.agg(...)`` returns one row per group, in order of
        each group's first input row."""
        return _GroupBy(self, keys)

    def join(self, other: _Frame, *, on: Any = None, how: str = "inner") -> Self:
        """Equi-join this frame with ``other`` on the shared key column(s) ``on``.

        ``on`` is a column name (or ``ur.col(name)``), or a list of them, present in
        both frames; ``how`` is ``"inner"``, ``"left"``, ``"right"``, ``"outer"``
        (also spelled ``"full"``), ``"semi"`` or ``"anti"``. ``other`` is any frame:
        a source, a graph column, a traversal, another join. The result keeps this
        frame's type (a NodeFrame stays a NodeFrame) and has this frame's columns,
        then ``other``'s non-key columns (none for ``semi``/``anti``); each key
        appears once, holding ``other``'s key on a row with no match in this frame.
        A null key matches nothing. This is a join between two arbitrary frames —
        distinct from the automatic attribute attach that
        ``edges.nodes().with_columns(...)`` does. A join changes the row/column set,
        so the frame starts a fresh topology index; a graph algorithm over joined
        edges runs over the topology of the joined rows.

        Rows come back in a fixed order. For ``inner``, ``left``, ``semi`` and
        ``anti``: this frame's rows in order, each followed by its matches in
        ``other``'s row order (an unmatched ``how="left"`` row once, with nulls).
        ``right`` is the mirror image: ``other``'s rows in order, each followed by
        its matches in this frame's order. ``outer`` is ``left``'s order, then
        ``other``'s unmatched rows in its order. A later ``sort`` breaks ties in
        that order.
        """
        if not isinstance(other, _Frame):
            raise TypeError(
                "join(other): other must be an ursa frame (EdgeFrame/NodeFrame); "
                f"got {type(other).__name__!r}."
            )
        on_keys, error = _ir.capture(_ir.join_keys, on)
        return self._derive(self._node.join(other._node, on_keys, how, error))

    # -- inspection ----------------------------------------------------------
    def explain(self, physical: bool = False) -> str:
        """The plan as text: the frame's steps as a tree, the schema it collects
        to, and the DataFusion plans a collect runs (optimized; with
        ``physical=True`` their physical plans too). Reads no rows and builds no
        index. The text is for reading: its format is not a stable API."""
        return self._node.explain(physical)

    def schema(self) -> Any:
        """The schema ``collect()`` returns, as a ``pyarrow.Schema`` (names, types
        and nullability), without running the plan. A scan reads its file's
        schema; nothing reads rows."""
        return self._node.schema()

    # -- materialization / egress --------------------------------------------
    # `collect()` is the one method every egress form below routes through.
    def collect(self) -> MaterializedFrame:
        from ._result import MaterializedFrame

        return MaterializedFrame(self._node.collect())

    def to_polars(self):  # -> polars.DataFrame
        """``collect()``, then a ``polars.DataFrame`` (zero-copy via Arrow).

        Needs the optional ``polars`` extra: ``pip install 'ursa-graph[polars]'``.
        ``to_arrow()`` / ``to_dicts()`` (or just ``print(frame.collect())``) need
        nothing beyond a bare install.
        """
        return self.collect().to_polars()

    def to_arrow(self):  # -> pyarrow.Table
        """``collect()``, then a ``pyarrow.Table`` (zero-copy; no extra needed)."""
        return self.collect().to_arrow()

    def to_dicts(self) -> list[dict[str, Any]]:
        """``collect()``, then a list of row dicts (no extra needed)."""
        return self.collect().to_dicts()

    def sink_parquet(self, path: str, **opts: Any) -> None:
        self.collect().sink_parquet(path, **opts)

    # No **opts: CSV write options aren't threaded through, so accepting them
    # would silently drop them (sink_parquet does forward its opts to pyarrow).
    def sink_csv(self, path: str) -> None:
        self.collect().sink_csv(path)

    def __repr__(self) -> str:
        return f"<{self.__class__.__name__} lazy; {self._node.n_steps} step(s)>"


_F = TypeVar("_F", bound=_Frame)


class _GroupBy(Generic[_F]):
    """Intermediate returned by ``group_by``; ``.agg(...)`` closes it into a frame
    of the grouped frame's type (grouped edges are still edges)."""

    def __init__(self, frame: _F, keys: tuple[Any, ...]) -> None:
        self._frame = frame
        self._keys = keys

    def agg(self, *exprs: Any, **named: Any) -> _F:
        # A grouped result replaces the schema, so (like sample/distinct/join) the
        # frame starts a fresh topology index: a graph op on the grouped frame
        # runs over the grouped rows' topology, never a parent's index.
        parsed, error = _ir.capture(_ir.group_agg, self._keys, exprs, named)
        keys, aggs = parsed if parsed is not None else (None, None)
        return self._frame._derive(self._frame._node.group_by_agg(keys, aggs, error))


class EdgeFrame(_Frame):
    """A frame with designated ``src`` and ``dst`` columns plus a cached topology
    index. **The EdgeFrame is the graph.**

    ``src=`` / ``dst=`` are *role mappings*, not renames: the original column
    names are preserved for round-tripping and exposed via ``src_col`` / ``dst_col``.
    """

    __slots__ = ()

    def __init__(self, data: Any, *, src: str, dst: str, on_null: str = "error") -> None:
        """Build an EdgeFrame from in-memory data, inferring the Arrow schema.

        ``data`` may be a list of row dicts, a dict of equal-length columns, a
        ``polars.DataFrame``, a ``pandas.DataFrame``, or a ``pyarrow.Table`` /
        ``RecordBatch`` — whatever ``_to_arrow_table`` normalizes. ``src`` / ``dst``
        are *role mappings* naming the endpoint columns, not renames.

        ``on_null`` sets the null-endpoint policy: ``"error"`` (default) leaves any
        null ``src``/``dst`` in place so the topology build raises; ``"drop"``
        filters those rows out here (emitting a count), before the endpoint arrays
        and the retained edge table are derived — so both stay row-aligned.

        Whatever the input flavour, the data is stored as one canonical Arrow
        source, so a frame built here is indistinguishable downstream from one
        built via ``from_arrow`` / ``from_polars`` / ``from_pandas`` or a
        ``scan_edges`` file — the load-bearing "same from here on" guarantee.
        """
        from ._io import (
            _drop_null_endpoint_rows,
            _extract_edge_arrays,
            _to_arrow_table,
            _validate_on_null,
            _warn_dropped_endpoints,
        )

        _validate_on_null(on_null)
        table = _to_arrow_table(data)
        if on_null == "drop":
            table, dropped = _drop_null_endpoint_rows(table, src, dst)
            _warn_dropped_endpoints(dropped)
        endpoints = _extract_edge_arrays(table, src, dst)
        self._node = _native().PlanNode.edge_table(table, endpoints, src, dst)

    @property
    def src_col(self) -> str:
        """Original column name playing the source role."""
        return self._node.src_col

    @property
    def dst_col(self) -> str:
        """Original column name playing the destination role."""
        return self._node.dst_col

    @property
    def _index_build_cell(self) -> Any:
        """The frame's index cell (``.value``: its built ``GraphIndex`` or None),
        shared with the derivations that keep its rows."""
        return self._node.index_cell

    _index_cell = _index_build_cell

    @property
    def _edge_table(self) -> Any | None:
        """The source's whole in-memory edge table (None for a scan)."""
        return self._node.source[0]

    @property
    def _edge_arrays(self) -> tuple[Any, Any] | None:
        """The source's canonical ``(src, dst)`` endpoint arrays (in-memory)."""
        return self._node.source[1]

    @property
    def _scan_spec(self) -> dict[str, Any] | None:
        """A scan source's storage spec (``storage_options``, ``store``)."""
        return self._node.source[2]

    def nodes(self) -> NodeFrame:
        """The distinct union of ``src`` and ``dst`` values, as a lazy NodeFrame."""
        return NodeFrame._wrap(self._node.nodes())

    def reverse(self) -> EdgeFrame:
        """Swap the src/dst roles. Metadata-only — no data movement.

        Graph ops on the reversed frame build the transpose CSR
        (``degree(edges.reverse(), 'out') == degree(edges, 'in')``). The index is
        built afresh on the swapped edges rather than reused (reusing the parent
        topology with flipped direction is a future optimization)."""
        return EdgeFrame._wrap(self._node.reverse())

    def __repr__(self) -> str:
        return (
            f"<EdgeFrame src={self.src_col!r} dst={self.dst_col!r} "
            f"lazy; {self._node.n_steps} step(s)>"
        )


class NodeFrame(_Frame):
    """A frame with a designated ``id`` column. An attribute table for the graph;
    carries no topology index.

    A frame built from an attribute table (``from_arrow``/``from_polars`` with
    ``id=``, or ``scan_nodes``) runs its steps over that table; ``collect()``
    left-joins algorithm outputs onto it by id. A frame from ``edges.nodes()`` has
    the edge frame's node set instead.
    """

    __slots__ = ()

    def __init__(self, data: Any, *, id: str) -> None:
        """Build a NodeFrame (attribute table) from in-memory data.

        ``data`` accepts the same flavours as ``EdgeFrame`` (row dicts, column
        dict, polars / pandas DataFrame, pyarrow Table/RecordBatch); ``id`` is the
        id-role mapping. Stored as one canonical Arrow attribute table, so it is
        indistinguishable downstream from a ``from_arrow(..., id=)`` frame."""
        from ._io import _node_attr_table, _to_arrow_table

        table = _node_attr_table(_to_arrow_table(data), id)
        self._node = _native().PlanNode.node_table(table, id)

    @property
    def id_col(self) -> str:
        """Original column name playing the id role."""
        return self._node.id_col

    @property
    def _attr_build_cell(self) -> _CacheView:
        """A ``scan_nodes`` frame's scanned-table cache (``.value``), shared with
        its derivations."""
        return _CacheView(self._node.node_cache)

    @property
    def _seeds_cell(self) -> _CacheView:
        """This frame's ids as a seed set (``.value``), once it had to run."""
        return _CacheView(self._node.cached_seeds)

    def __repr__(self) -> str:
        return f"<NodeFrame id={self.id_col!r} lazy; {self._node.n_steps} step(s)>"
