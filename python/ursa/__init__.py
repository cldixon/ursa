"""Ursa — Polars-shaped dataframes for graph data.

    import ursa as ur

    edges = ur.scan_edges("web-google.csv", src="FromNodeId", dst="ToNodeId")
    nodes = edges.nodes()
    top = (
        nodes
        .with_columns(
            pagerank  = ur.pagerank(edges, damping=0.85),
            in_degree = ur.degree(edges, direction="in"),
        )
        .sort("pagerank", descending=True)
        .head(20)
        .collect()
    )

This is the v0.1 **skeleton**. The public surface below mirrors the design spec;
most of it raises ``NotImplementedError`` pending the DataFusion plan-building
layer. What *is* live: the expression dialect (``ur.col`` & friends, pure Python)
and — when the native extension is built — a ``ur.demo`` namespace that runs the
real Rust kernels over in-memory edge lists, proving the full Python→Rust→kernel
path.
"""

from __future__ import annotations

# --- Expression dialect (pure Python, always available) ---------------------
from ._expr import Expr, col, lit, src, dst, id  # noqa: A004  (id mirrors ur.id())

# --- IO constructors --------------------------------------------------------
from ._io import (
    scan_edges,
    scan_nodes,
    read_edges,
    read_nodes,
    from_polars,
    from_arrow,
)

# --- Frame types ------------------------------------------------------------
from ._frames import EdgeFrame, NodeFrame

# --- Graph verbs & algorithms ----------------------------------------------
from ._graph import (
    degree,
    neighbors,
    hop,
    shortest_path,
    random_walk,
    pagerank,
    connected_components,
    triangle_count,
    clustering_coefficient,
    betweenness,
    closeness,
    label_propagation,
    louvain,
)

# --- Graph-level statistics -------------------------------------------------
from ._stats import describe, density, avg_path_length, diameter

# --- Native extension (optional until built) --------------------------------
try:
    from . import _ursa as _native

    __core_version__ = _native.__core_version()
    from . import demo  # noqa: F401  (thin wrappers over the native demo kernels)

    _NATIVE_AVAILABLE = True
except ImportError:  # pragma: no cover - native module not yet built
    _native = None
    __core_version__ = None
    _NATIVE_AVAILABLE = False

__version__ = "0.1.0"

__all__ = [
    "Expr", "col", "lit", "src", "dst", "id",
    "scan_edges", "scan_nodes", "read_edges", "read_nodes", "from_polars", "from_arrow",
    "EdgeFrame", "NodeFrame",
    "degree", "neighbors", "hop", "shortest_path", "random_walk",
    "pagerank", "connected_components", "triangle_count", "clustering_coefficient",
    "betweenness", "closeness", "label_propagation", "louvain",
    "describe", "density", "avg_path_length", "diameter",
]
