"""Graph verbs and algorithms — members of the expression family.

Two positioning rules from the spec:

- **Traversals** (``hop``, ``shortest_path``, ``random_walk``) are frame-valued:
  a hop is EdgeFrame -> EdgeFrame, so traversal composes.
- **Node-valued algorithms** (``pagerank``, ``connected_components``,
  centralities, ...) are *dual-positioned*: the same kernel is either an
  expression inside ``with_columns`` or a standalone call returning a NodeFrame
  of ``(id, value)``. Both spellings lower to the same ``GraphAlgorithmNode``.

Topology is always threaded explicitly as the ``edges`` argument — there is no
ambient graph. Direction is a per-op parameter; weight is any expression over
edge columns.

These build ``Expr("graph", ...)`` / frame plan nodes; ``collect()`` executes
them through the ``ursa-plan`` physical operators (``ursa-core`` kernels behind
them).
"""

from __future__ import annotations

from typing import Any

from ._expr import Expr
from ._frames import EdgeFrame, NodeFrame, _PlanStep


class GraphExpr(Expr):
    """A node-valued graph algorithm — the spec's *dual-positioned* kernel.

    Used inside ``with_columns(pr=ur.pagerank(edges))`` it reads as an expression
    (the column definition). Used standalone it behaves as a lazy ``NodeFrame`` of
    ``(id, value)``: any frame method promotes it to
    ``edges.nodes().with_columns(<verb>=self)`` and delegates, so
    ``ur.pagerank(edges).filter(...).sort(...).collect()`` composes like any frame.
    """

    def _frame(self) -> NodeFrame:
        edges = self.payload["edges"]
        return edges.nodes().with_columns(**{self.payload["verb"]: self})

    def filter(self, predicate: Any) -> NodeFrame:
        return self._frame().filter(predicate)

    def with_columns(self, **exprs: Any) -> NodeFrame:
        return self._frame().with_columns(**exprs)

    def select(self, *columns: Any) -> NodeFrame:
        return self._frame().select(*columns)

    def sort(self, by: Any, *, descending: bool = False) -> NodeFrame:
        return self._frame().sort(by, descending=descending)

    def head(self, n: int = 10) -> NodeFrame:
        return self._frame().head(n)

    def distinct(self) -> NodeFrame:
        return self._frame().distinct()

    def collect(self) -> Any:
        return self._frame().collect()

    def to_polars(self) -> Any:
        return self._frame().to_polars()

    def to_arrow(self) -> Any:
        return self._frame().to_arrow()

    def to_dicts(self) -> list[dict[str, Any]]:
        return self._frame().to_dicts()

    def sink_parquet(self, path: str, **opts: Any) -> None:
        self._frame().sink_parquet(path, **opts)

    def sink_csv(self, path: str) -> None:
        self._frame().sink_csv(path)

    def explain(self) -> str:
        return self._frame().explain()


def _graph_expr(verb: str, **params: Any) -> GraphExpr:
    return GraphExpr("graph", {"verb": verb, **params})


class LayoutExpr(GraphExpr):
    """A layout — the first kernel that yields *two* columns from one run.

    Positions are two plain ``f32`` columns rather than a struct, because struct
    values and ``unnest`` do not exist in the dialect and a layout is a poor reason
    to invent them. So a layout is selected one axis at a time::

        nodes.with_columns(x=ur.layout_fr(edges).x, y=ur.layout_fr(edges).y)

    Both spellings carry identical parameters, so the engine keys them to the same
    computation and **runs the simulation once** (#115) — the two accessors pick
    different outputs of one result, not two results.

    Used bare it behaves like the other dual-positioned kernels, promoting to a
    NodeFrame of ``(id, x, y)``.
    """

    def _axis(self, field: int) -> GraphExpr:
        return GraphExpr("graph", {**self.payload, "field": field})

    @property
    def x(self) -> GraphExpr:
        """The x column."""
        return self._axis(0)

    @property
    def y(self) -> GraphExpr:
        """The y column."""
        return self._axis(1)

    def _frame(self) -> NodeFrame:
        edges = self.payload["edges"]
        return edges.nodes().with_columns(x=self.x, y=self.y)


def _layout_expr(verb: str, **params: Any) -> LayoutExpr:
    return LayoutExpr("graph", {"verb": verb, **params})


# --- traversal verbs (frame-valued) ----------------------------------------
class _NeighborAgg:
    """Returned by ``ur.neighbors(edges)``; ``.agg(expr)`` closes it into an Expr.

    Attribute-resolution rule: topology comes from the threaded ``edges``;
    attribute columns in ``expr`` resolve against the ambient frame the
    expression runs in (override with ``from_=``).
    """

    def __init__(self, edges: EdgeFrame, direction: str, from_: Any) -> None:
        self._edges, self._direction, self._from = edges, direction, from_

    def agg(self, expr: Expr, dtype: str = "f64") -> GraphExpr:
        """Close the neighbour aggregation into a graph expression. ``dtype="f32"``
        emits the aggregated column as 32-bit float."""
        return _graph_expr(
            "neighbors_agg",
            edges=self._edges,
            direction=self._direction,
            from_=self._from,
            agg=expr,
            dtype=dtype,
        )


def neighbors(edges: EdgeFrame, direction: str = "out", from_: Any = None) -> _NeighborAgg:
    """Neighbour aggregation. Fused by the optimizer into a segmented CSR reduction."""
    return _NeighborAgg(edges, direction, from_)


def degree(edges: EdgeFrame, direction: str = "out") -> GraphExpr:
    """Per-node degree (UInt32). ``direction`` is 'out' | 'in' | 'both'."""
    return _graph_expr("degree", edges=edges, direction=direction)


class _Hop:
    """Returned by ``ur.hop(...)``; frame-positioned. ``.from_(seeds)`` restricts
    the seed set; the result is a lazy EdgeFrame (src = seed, dst = reached)."""

    def __init__(self, edges: EdgeFrame, n: int, direction: str, seeds: Any = None) -> None:
        self._edges, self._n, self._direction, self._seeds = edges, n, direction, seeds

    def from_(self, seeds: Any) -> EdgeFrame:
        # Binding the seed set completes the traversal, so return the materialized
        # lazy EdgeFrame directly (not another `_Hop`). This is the frame every verb
        # composes on — a relational tail (`.filter`/`.collect`/...) and, since #116,
        # a node-valued graph op (`ur.pagerank(ur.hop(e, n).from_(seeds))`), which
        # needs a real `EdgeFrame` rather than the `__getattr__`-delegating builder.
        return _Hop(self._edges, self._n, self._direction, seeds)._materialize()

    def distinct(self) -> EdgeFrame:
        return self._materialize().distinct()

    def _materialize(self) -> EdgeFrame:
        # Carry the parent edges into the step so collect() can resolve (src, dst);
        # the produced EdgeFrame's own source/scan are empty (its rows are the
        # hop's reached edges, not the parent graph's).
        step = _PlanStep(
            "hop",
            {
                "n": self._n,
                "direction": self._direction,
                "seeds": self._seeds,
                "edges": self._edges,
            },
        )
        # The collected batch has literal ``src``/``dst`` columns (seed -> reached),
        # so the frame advertises those names — a tail filter references ``src``/
        # ``dst``, matching what it actually gets. (The parent's role names aren't
        # carried through a traversal.)
        return EdgeFrame._construct(src_col="src", dst_col="dst", plan=(step,))

    def collect(self):
        return self._materialize().collect()

    def __getattr__(self, name: str) -> Any:  # delegate EdgeFrame verbs onto the hop
        return getattr(self._materialize(), name)


def hop(edges: EdgeFrame, n: int = 1, direction: str = "out") -> _Hop:
    """``n``-hop expansion. Returns a lazy EdgeFrame so traversal composes."""
    return _Hop(edges, n, direction)


def shortest_path(
    edges: EdgeFrame,
    source: Any,
    target: Any,
    weight: Expr | None = None,
    direction: str = "out",
) -> EdgeFrame:
    """Single-pair shortest path. Returns an EdgeFrame ``(src, dst, hop, cost)``: one
    row per edge on the path, in order, with ``hop`` the 0-based position and ``cost``
    the cumulative path cost from ``source`` to that edge's destination. ``weight``
    selects minimum-cost (Dijkstra) over the edge-weight expression; omit it for
    unweighted BFS. For a weighted path ``cost`` is the summed edge weight (so the
    final row's ``cost`` is the total path cost); for an unweighted path it is the
    hop count (``hop + 1``), kept for schema uniformity."""
    step = _PlanStep(
        "shortest_path",
        {
            "source": source,
            "target": target,
            "weight": weight,
            "direction": direction,
            "edges": edges,
        },
    )
    # Result columns are literal ``src``/``dst`` (one row per edge on the path);
    # advertise those names so a tail filter matches (see _Hop._materialize).
    return EdgeFrame._construct(src_col="src", dst_col="dst", plan=(step,))


def random_walk(
    edges: EdgeFrame,
    start: Any,
    steps: int,
    walks_per_node: int = 1,
    seed: int | None = None,
) -> NodeFrame:
    """Random walks; a frame of ``(walk_id, step, node)`` — feeds node2vec-style
    embedding pipelines directly."""
    step = _PlanStep(
        "random_walk",
        {
            "start": start,
            "steps": steps,
            "walks_per_node": walks_per_node,
            "seed": seed,
            "edges": edges,
        },
    )
    return NodeFrame._construct(id_col="node", plan=(step,))


# --- node-valued algorithms (dual-positioned) ------------------------------
# In `with_columns(...)` they read as expressions; called bare they return a
# NodeFrame of (id, value) — the standalone spelling promotes the Expr to
# `edges.nodes().with_columns(<verb>=self)` (see GraphExpr._frame).


def pagerank(
    edges: EdgeFrame,
    damping: float = 0.85,
    max_iter: int = 30,
    tol: float = 1e-6,
    weight: Expr | None = None,
    dtype: str = "f64",
) -> GraphExpr:
    """PageRank (pull-based fixpoint). ``dtype="f32"`` emits the score column as
    32-bit float (half the size; the fixpoint still iterates in f64)."""
    return _graph_expr(
        "pagerank",
        edges=edges,
        damping=damping,
        max_iter=max_iter,
        tol=tol,
        weight=weight,
        dtype=dtype,
    )


def connected_components(edges: EdgeFrame, mode: str = "weak") -> GraphExpr:
    """Connected components, one integer component label per node.

    ``mode='weak'`` (default) treats edges as undirected: two nodes share a label if
    a path connects them ignoring direction. ``mode='strong'`` returns strongly
    connected components: two nodes share a label only if each is reachable from the
    other following edge direction. Labels are arbitrary but stable ids (group nodes
    by label to get the components)."""
    return _graph_expr("connected_components", edges=edges, mode=mode)


def triangle_count(edges: EdgeFrame) -> GraphExpr:
    """Per-node triangle count (treats edges as undirected)."""
    return _graph_expr("triangle_count", edges=edges)


def clustering_coefficient(edges: EdgeFrame, dtype: str = "f64") -> GraphExpr:
    """Local clustering coefficient (derived from triangles). ``dtype="f32"`` emits
    the coefficient column as 32-bit float."""
    return _graph_expr("clustering_coefficient", edges=edges, dtype=dtype)


def betweenness(
    edges: EdgeFrame,
    sample: float | None = None,
    weight: Expr | None = None,
    seed: int | None = None,
    dtype: str = "f64",
) -> GraphExpr:
    """Betweenness centrality (Brandes), directed and unnormalized. ``sample=``
    approximates from a ``seed``-shuffled subset of sources (exact is O(nm));
    ``seed`` makes the sampled estimate reproducible. Parallel edges count as
    distinct shortest paths (so a multigraph diverges from a simple-graph
    reference like NetworkX); weighted, only *exactly* float-equal path costs tie.
    ``dtype="f32"`` emits the centrality column as 32-bit float."""
    return _graph_expr(
        "betweenness", edges=edges, sample=sample, weight=weight, seed=seed, dtype=dtype
    )


def closeness(edges: EdgeFrame, weight: Expr | None = None, dtype: str = "f64") -> GraphExpr:
    """Closeness centrality. ``dtype="f32"`` emits the score column as 32-bit float."""
    return _graph_expr("closeness", edges=edges, weight=weight, dtype=dtype)


def label_propagation(edges: EdgeFrame, max_iter: int = 20, seed: int | None = None) -> GraphExpr:
    """Community detection via label propagation."""
    return _graph_expr("label_propagation", edges=edges, max_iter=max_iter, seed=seed)


def louvain(
    edges: EdgeFrame, weight: Expr | None = None, resolution: float = 1.0, seed: int | None = None
) -> GraphExpr:
    """Community detection via Louvain modularity optimization."""
    return _graph_expr("louvain", edges=edges, weight=weight, resolution=resolution, seed=seed)


# --- layout (multi-output) --------------------------------------------------
# A force-directed layout is an iterative fixpoint over the CSR — the same
# computational shape as PageRank — so it lives with the kernels rather than in a
# rendering library, and positions come back as ordinary columns: filter them,
# join them, `sink_parquet` them. Precomputing a layout once and caching it in the
# lake is a workflow, not a workaround.


def layout_fa2(
    edges: EdgeFrame,
    iterations: int = 300,
    k: float = 1.0,
    gravity: float = 1.0,
    strong_gravity: bool = False,
    lin_log: bool = False,
    theta: float = 0.5,
    jitter_tolerance: float = 1.0,
    seed: int | None = None,
) -> LayoutExpr:
    """ForceAtlas2 layout; ``(x, y)`` as two ``f32`` columns.

    The layout to reach for on a real graph. Three things distinguish it from
    :func:`layout_fr`:

    * **Repulsion is degree-weighted**, so a hub pushes proportionally to what it
      anchors instead of being buried inside its own neighbourhood.
    * **Attraction is linear** in distance rather than quadratic, so clusters stay
      legible instead of collapsing.
    * **The step size adapts** per node from how much each one is oscillating,
      rather than following a fixed cooling schedule that has to suit the graph.

    Repulsion is approximated with a Barnes-Hut quadtree, giving O(n log n) per
    iteration. ``theta`` is the opening angle: smaller is more accurate and slower,
    and ``0.0`` is exact all-pairs.

    ``gravity`` pulls toward the origin so disconnected components stay in one
    picture; ``strong_gravity`` makes that pull grow with distance, which tightens a
    drawing that would otherwise sprawl. ``lin_log`` switches attraction to
    ``ln(1 + d)``, separating clusters more distinctly.

    Deterministic: a given ``seed`` reproduces a layout exactly, and the result is
    bit-identical across thread counts. Select an axis with ``.x`` / ``.y``; both
    share one simulation.

    .. note::
       ``lin_log=True`` is reproducible on one machine but **not** guaranteed
       bit-identical across platforms — it is the only mode that uses a
       transcendental (``ln``), whose last bit is a libm implementation detail.
    """
    return _layout_expr(
        "layout_fa2",
        edges=edges,
        iterations=iterations,
        k=k,
        gravity=gravity,
        strong_gravity=strong_gravity,
        lin_log=lin_log,
        theta=theta,
        jitter_tolerance=jitter_tolerance,
        seed=seed,
    )


def layout_fr(
    edges: EdgeFrame,
    iterations: int = 300,
    k: float = 1.0,
    gravity: float = 0.02,
    theta: float = 0.5,
    seed: int | None = None,
) -> LayoutExpr:
    """Fruchterman-Reingold layout; ``(x, y)`` as two ``f32`` columns.

    The simpler force model: quadratic attraction, repulsion that ignores degree,
    and a fixed cooling schedule. :func:`layout_fa2` is the better default on a real
    graph; this one is easier to reason about and is the baseline the tests pin.

    ``k`` is the ideal edge length and sets the units the result comes out in;
    ``gravity`` pulls the drawing toward the origin, which is what keeps
    disconnected components in one picture instead of repelling forever. ``theta``
    is the Barnes-Hut opening angle, as in :func:`layout_fa2`.

    Deterministic: a given ``seed`` reproduces a layout exactly, and the result is
    bit-identical across thread counts. Select an axis with ``.x`` / ``.y``; both
    share one simulation.
    """
    return _layout_expr(
        "layout_fr",
        edges=edges,
        iterations=iterations,
        k=k,
        gravity=gravity,
        theta=theta,
        seed=seed,
    )


def layout_random(edges: EdgeFrame, k: float = 1.0, seed: int | None = None) -> LayoutExpr:
    """A deterministic spread over a phyllotactic spiral; ``(x, y)`` as two columns.

    No two nodes coincide, which is what makes it a safe starting point for a force
    layout — two nodes at one point feel unbounded repulsion.
    """
    return _layout_expr("layout_random", edges=edges, k=k, seed=seed)


def layout_circle(edges: EdgeFrame, k: float = 1.0) -> LayoutExpr:
    """Nodes evenly spaced on a circle in id order; ``(x, y)`` as two columns.

    A baseline that is obviously not force-directed: if a figure looks the same
    under this and under ``layout_fa2``, the force layout did not run.
    """
    return _layout_expr("layout_circle", edges=edges, k=k)
