"""The two public types: ``EdgeFrame`` and ``NodeFrame`` — both lazy.

There is no ``Graph`` object. The EdgeFrame *is* the graph (designated ``src`` /
``dst`` roles plus an internal cached topology index); a NodeFrame is an
attribute table with a designated ``id`` role that relates to it by join
convention. A frame *is* a logical plan until ``.collect()``.

Skeleton behaviour: relational transformations accumulate onto an in-Python
logical-plan chain (inspectable via ``.explain()``); the operations that require
the engine — ``collect``, the ``to_*`` egress, ``sink_*`` — raise
``NotImplementedError`` until the ``ursa-plan`` / DataFusion layer is wired.
Role tracking, ``.nodes()``, ``.reverse()``, and the index-preservation contract
metadata are modelled here so the shape is exercisable now.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    # typing.Self is 3.11+; typing_extensions backports it for our 3.10 floor.
    from typing_extensions import Self

    from ._result import MaterializedFrame

_ENGINE_TODO = (
    "requires the DataFusion execution engine (ursa-plan), not yet wired in this "
    "skeleton. The topology index + kernels (ursa-core) and the plan builder "
    "(this module) are in place; collect() is the next integration step."
)


@dataclass(frozen=True)
class _PlanStep:
    """One logical operation in a frame's plan (kept human-readable for explain())."""

    op: str
    args: dict[str, Any] = field(default_factory=dict)

    def __repr__(self) -> str:
        inner = ", ".join(f"{k}={v!r}" for k, v in self.args.items())
        return f"{self.op}({inner})"


class _Frame:
    """Shared relational surface for both frame types.

    ``_plan`` is the accumulated chain of logical steps. ``_has_index`` tracks the
    index-preservation contract at the metadata level: property-only ops preserve
    it, row-changing ops drop it (rebuilt lazily on the next graph op).
    """

    __slots__ = ("_has_index", "_plan")

    def __init__(self, plan: tuple[_PlanStep, ...], has_index: bool) -> None:
        self._plan = plan
        self._has_index = has_index

    # -- internal helpers ----------------------------------------------------
    # Chainable verbs return `Self`, so `EdgeFrame.filter(...)` stays an
    # `EdgeFrame` (and NodeFrame a NodeFrame) through a whole pipeline.
    def _extend(self, step: _PlanStep, *, drops_index: bool = False) -> Self:
        new = self.__class__.__new__(self.__class__)
        _Frame.__init__(
            new,
            (*self._plan, step),
            has_index=self._has_index and not drops_index,
        )
        return new

    # -- relational verbs (property-preserving unless noted) -----------------
    def filter(self, predicate: Any) -> Self:
        # Row-changing on an EdgeFrame => drops the topology index (rebuilt lazily).
        return self._extend(_PlanStep("filter", {"predicate": predicate}), drops_index=True)

    def select(self, *columns: Any) -> Self:
        return self._extend(_PlanStep("select", {"columns": columns}))

    def with_columns(self, **exprs: Any) -> Self:
        return self._extend(_PlanStep("with_columns", {"exprs": exprs}))

    def rename(self, mapping: dict[str, str]) -> Self:
        return self._extend(_PlanStep("rename", {"mapping": mapping}))

    def sort(self, by: Any, *, descending: bool = False) -> Self:
        return self._extend(_PlanStep("sort", {"by": by, "descending": descending}))

    def head(self, n: int = 10) -> Self:
        return self._extend(_PlanStep("head", {"n": n}))

    limit = head

    def sample(self, n: int, *, seed: int | None = None) -> Self:
        return self._extend(_PlanStep("sample", {"n": n, "seed": seed}))

    def distinct(self) -> Self:
        return self._extend(_PlanStep("distinct", {}), drops_index=True)

    def group_by(self, *keys: Any) -> _GroupBy:
        return _GroupBy(self, keys)

    def join(self, other: _Frame, *, on: Any = None, how: str = "inner") -> Self:
        return self._extend(_PlanStep("join", {"on": on, "how": how}))

    # -- inspection ----------------------------------------------------------
    def explain(self) -> str:
        lines = [f"  {i}: {step!r}" for i, step in enumerate(self._plan)]
        idx = "index: preserved" if self._has_index else "index: dropped (rebuild lazily)"
        return f"{self.__class__.__name__} plan [{idx}]\n" + "\n".join(lines)

    def schema(self) -> dict[str, str]:
        raise NotImplementedError(f"schema() {_ENGINE_TODO}")

    # -- materialization / egress (engine required) --------------------------
    def collect(self) -> Self:
        raise NotImplementedError(f"collect() {_ENGINE_TODO}")

    def to_polars(self):  # -> polars.DataFrame
        raise NotImplementedError(f"to_polars() {_ENGINE_TODO}")

    def to_arrow(self):  # -> pyarrow.Table
        raise NotImplementedError(f"to_arrow() {_ENGINE_TODO}")

    def to_dicts(self) -> list[dict[str, Any]]:
        raise NotImplementedError(f"to_dicts() {_ENGINE_TODO}")

    def sink_parquet(self, path: str, **opts: Any) -> None:
        raise NotImplementedError(f"sink_parquet() {_ENGINE_TODO}")

    def sink_csv(self, path: str, **opts: Any) -> None:
        raise NotImplementedError(f"sink_csv() {_ENGINE_TODO}")

    def __repr__(self) -> str:
        return f"<{self.__class__.__name__} lazy; {len(self._plan)} step(s)>"


class _GroupBy:
    """Intermediate returned by ``group_by``; ``.agg(...)`` closes it into a frame."""

    def __init__(self, frame: _Frame, keys: tuple[Any, ...]) -> None:
        self._frame = frame
        self._keys = keys

    def agg(self, *exprs: Any, **named: Any) -> _Frame:
        return self._frame._extend(
            _PlanStep("group_by_agg", {"keys": self._keys, "exprs": exprs, "named": named})
        )


class EdgeFrame(_Frame):
    """A frame with designated ``src`` and ``dst`` columns plus a cached topology
    index. **The EdgeFrame is the graph.**

    ``src=`` / ``dst=`` are *role mappings*, not renames: the original column
    names are preserved for round-tripping and exposed via ``src_col`` / ``dst_col``.
    """

    # `_source` carries the actual in-memory Arrow endpoint arrays for frames
    # built via `from_arrow`/`from_polars`. `_scan` carries a file-source spec
    # ({path, src, dst}) for frames built via `scan_edges`; `collect()` resolves
    # it through a DataFusion scan. A frame has at most one of them.
    #
    # `_index` is the built native topology index (a `GraphIndex` handle), lazily
    # cached on first graph op and shared by every subsequent op over this frame
    # (the spec's index-preservation contract). It rides the same preserve/drop
    # rail as `_source`/`_scan`: property-only ops keep it, structural ops drop it.
    __slots__ = ("_dst_col", "_index", "_scan", "_source", "_src_col")

    def __init__(
        self,
        src_col: str,
        dst_col: str,
        plan: tuple[_PlanStep, ...] = (),
        has_index: bool = True,
        source: tuple[Any, Any] | None = None,
        scan: dict[str, Any] | None = None,
        index: Any | None = None,
    ) -> None:
        super().__init__(plan, has_index)
        self._src_col = src_col
        self._dst_col = dst_col
        self._source = source
        self._scan = scan
        self._index = index

    @property
    def _edge_arrays(self) -> tuple[Any, Any] | None:
        """The (src, dst) Arrow arrays if this frame has an in-memory source."""
        return self._source

    @property
    def _scan_spec(self) -> dict[str, Any] | None:
        """The file-source spec if this frame was built via ``scan_edges``."""
        return self._scan

    @property
    def src_col(self) -> str:
        """Original column name playing the source role."""
        return self._src_col

    @property
    def dst_col(self) -> str:
        """Original column name playing the destination role."""
        return self._dst_col

    def _extend(self, step: _PlanStep, *, drops_index: bool = False) -> EdgeFrame:
        # A row-changing op invalidates the in-memory source / scan spec and the
        # cached topology index (they no longer match the frame); property-only
        # ops keep them.
        return EdgeFrame(
            self._src_col,
            self._dst_col,
            (*self._plan, step),
            has_index=self._has_index and not drops_index,
            source=None if drops_index else self._source,
            scan=None if drops_index else self._scan,
            index=None if drops_index else self._index,
        )

    def nodes(self) -> NodeFrame:
        """The distinct union of ``src`` and ``dst`` values, as a lazy NodeFrame."""
        return NodeFrame(
            id_col="id",
            plan=(*self._plan, _PlanStep("nodes", {"from": (self._src_col, self._dst_col)})),
        )

    def reverse(self) -> EdgeFrame:
        """Swap the src/dst roles. Metadata-only — no data movement."""
        return EdgeFrame(
            self._dst_col,
            self._src_col,
            (*self._plan, _PlanStep("reverse", {})),
            has_index=False,  # transpose becomes the new "out" direction
        )

    # A frame-positioned traversal (ur.hop) executes here, returning the reached
    # edges as a materialized frame. A plain edge frame with no traversal step is
    # not collectable on its own (call an algorithm on it) — collect_edge_frame
    # raises a clear error in that case.
    def collect(self) -> MaterializedFrame:  # ty: ignore[invalid-method-override]
        from ._execute import collect_edge_frame

        return collect_edge_frame(self)

    def to_polars(self) -> Any:
        return self.collect().to_polars()

    def to_arrow(self) -> Any:
        return self.collect().to_arrow()

    def to_dicts(self) -> list[dict[str, Any]]:
        return self.collect().to_dicts()

    def sink_parquet(self, path: str, **opts: Any) -> None:
        self.collect().sink_parquet(path, **opts)

    def sink_csv(self, path: str, **opts: Any) -> None:
        self.collect().sink_csv(path)

    def __repr__(self) -> str:
        return (
            f"<EdgeFrame src={self._src_col!r} dst={self._dst_col!r} "
            f"lazy; {len(self._plan)} step(s)>"
        )


class NodeFrame(_Frame):
    """A frame with a designated ``id`` column. An attribute table for the graph;
    carries no topology index.

    ``_source`` holds the in-memory Arrow attribute table (id + attribute columns)
    for frames built via ``from_arrow``/``from_polars`` with ``id=``; ``_scan``
    holds a file-source spec (``{path, id}``) for frames built via ``scan_nodes``,
    which ``collect()`` materializes through a DataFusion scan. A frame has at most
    one of them. ``collect()`` left-joins algorithm outputs onto the attribute
    table by id. NodeFrames derived from ``edges.nodes()`` carry neither.
    """

    __slots__ = ("_id_col", "_scan", "_source")

    def __init__(
        self,
        id_col: str,
        plan: tuple[_PlanStep, ...] = (),
        source: Any | None = None,
        scan: dict[str, Any] | None = None,
    ) -> None:
        super().__init__(plan, has_index=False)
        self._id_col = id_col
        self._source = source
        self._scan = scan

    @property
    def id_col(self) -> str:
        """Original column name playing the id role."""
        return self._id_col

    @property
    def _attr_table(self) -> Any | None:
        """The in-memory Arrow attribute table, if this frame has one."""
        return self._source

    @property
    def _scan_spec(self) -> dict[str, Any] | None:
        """The file-source spec if this frame was built via ``scan_nodes``."""
        return self._scan

    def _extend(self, step: _PlanStep, *, drops_index: bool = False) -> NodeFrame:
        return NodeFrame(self._id_col, (*self._plan, step), source=self._source, scan=self._scan)

    # Composed pipelines (with_columns of graph algorithms + filter/sort/head)
    # execute here. Returns the materialized result rather than another lazy
    # frame, so the override signature intentionally differs from the base.
    def collect(self) -> MaterializedFrame:  # ty: ignore[invalid-method-override]
        from ._execute import collect_node_frame

        return collect_node_frame(self)

    def to_polars(self) -> Any:
        return self.collect().to_polars()

    def to_arrow(self) -> Any:
        return self.collect().to_arrow()

    def to_dicts(self) -> list[dict[str, Any]]:
        return self.collect().to_dicts()

    def sink_parquet(self, path: str, **opts: Any) -> None:
        self.collect().sink_parquet(path, **opts)

    def sink_csv(self, path: str, **opts: Any) -> None:
        self.collect().sink_csv(path)

    def __repr__(self) -> str:
        return f"<NodeFrame id={self._id_col!r} lazy; {len(self._plan)} step(s)>"
