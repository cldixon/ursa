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

Skeleton: these build ``Expr("graph", ...)`` / frame plan nodes. Their execution
is the ``ursa-plan`` physical operators (``ursa-core`` kernels behind them).
"""

from __future__ import annotations

from typing import Any

from ._expr import Expr
from ._frames import EdgeFrame, NodeFrame, _PlanStep


def _graph_expr(verb: str, **params: Any) -> Expr:
    return Expr("graph", {"verb": verb, **params})


# --- traversal verbs (frame-valued) ----------------------------------------
class _NeighborAgg:
    """Returned by ``ur.neighbors(edges)``; ``.agg(expr)`` closes it into an Expr.

    Attribute-resolution rule: topology comes from the threaded ``edges``;
    attribute columns in ``expr`` resolve against the ambient frame the
    expression runs in (override with ``from_=``).
    """

    def __init__(self, edges: EdgeFrame, direction: str, from_: Any) -> None:
        self._edges, self._direction, self._from = edges, direction, from_

    def agg(self, expr: Expr) -> Expr:
        return _graph_expr("neighbors_agg", direction=self._direction, from_=self._from, agg=expr)


def neighbors(edges: EdgeFrame, direction: str = "out", from_: Any = None) -> _NeighborAgg:
    """Neighbour aggregation. Fused by the optimizer into a segmented CSR reduction."""
    return _NeighborAgg(edges, direction, from_)


def degree(edges: EdgeFrame, direction: str = "out") -> Expr:
    """Per-node degree (UInt32). ``direction`` is 'out' | 'in' | 'both'."""
    return _graph_expr("degree", edges=edges, direction=direction)


class _Hop:
    """Returned by ``ur.hop(...)``; frame-positioned. ``.from_(seeds)`` restricts
    the seed set; the result is a lazy EdgeFrame (src = seed, dst = reached)."""

    def __init__(self, edges: EdgeFrame, n: int, direction: str, seeds: Any = None) -> None:
        self._edges, self._n, self._direction, self._seeds = edges, n, direction, seeds

    def from_(self, seeds: Any) -> _Hop:
        return _Hop(self._edges, self._n, self._direction, seeds)

    def distinct(self) -> EdgeFrame:
        return self._materialize().distinct()

    def _materialize(self) -> EdgeFrame:
        step = _PlanStep("hop", {"n": self._n, "direction": self._direction, "seeds": self._seeds})
        return EdgeFrame(src_col=self._edges.src_col, dst_col=self._edges.dst_col, plan=(step,))

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
    """Single-pair shortest path. Returns an EdgeFrame: one row per edge on the
    path, in order, with a ``hop`` column (and a cost column when weighted).
    Omit ``weight`` for unweighted BFS."""
    step = _PlanStep(
        "shortest_path",
        {"source": source, "target": target, "weight": weight, "direction": direction},
    )
    return EdgeFrame(src_col=edges.src_col, dst_col=edges.dst_col, plan=(step,))


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
        {"start": start, "steps": steps, "walks_per_node": walks_per_node, "seed": seed},
    )
    return NodeFrame(id_col="node", plan=(step,))


# --- node-valued algorithms (dual-positioned) ------------------------------
# In `with_columns(...)` they read as expressions; called bare they return a
# NodeFrame of (id, value). The skeleton returns the Expr form; the standalone
# NodeFrame spelling is produced by the same node at plan-build time.


def pagerank(
    edges: EdgeFrame,
    damping: float = 0.85,
    max_iter: int = 30,
    tol: float = 1e-6,
    weight: Expr | None = None,
) -> Expr:
    """PageRank (pull-based fixpoint)."""
    return _graph_expr(
        "pagerank", edges=edges, damping=damping, max_iter=max_iter, tol=tol, weight=weight
    )


def connected_components(edges: EdgeFrame, mode: str = "weak") -> Expr:
    """Connected components. ``mode='weak'`` for v0.1 (``'strong'`` later)."""
    return _graph_expr("connected_components", edges=edges, mode=mode)


def triangle_count(edges: EdgeFrame) -> Expr:
    """Per-node triangle count (treats edges as undirected)."""
    return _graph_expr("triangle_count", edges=edges)


def clustering_coefficient(edges: EdgeFrame) -> Expr:
    """Local clustering coefficient (derived from triangles)."""
    return _graph_expr("clustering_coefficient", edges=edges)


def betweenness(edges: EdgeFrame, sample: float | None = None, weight: Expr | None = None) -> Expr:
    """Betweenness centrality (Brandes). ``sample=`` approximates; exact is O(nm)."""
    return _graph_expr("betweenness", edges=edges, sample=sample, weight=weight)


def closeness(edges: EdgeFrame, weight: Expr | None = None) -> Expr:
    """Closeness centrality."""
    return _graph_expr("closeness", edges=edges, weight=weight)


def label_propagation(edges: EdgeFrame, max_iter: int = 20, seed: int | None = None) -> Expr:
    """Community detection via label propagation."""
    return _graph_expr("label_propagation", edges=edges, max_iter=max_iter, seed=seed)


def louvain(
    edges: EdgeFrame, weight: Expr | None = None, resolution: float = 1.0, seed: int | None = None
) -> Expr:
    """Community detection via Louvain modularity optimization."""
    return _graph_expr("louvain", edges=edges, weight=weight, resolution=resolution, seed=seed)
