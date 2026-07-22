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

from importlib.metadata import PackageNotFoundError, version
from types import ModuleType

# --- Expression dialect (pure Python, always available) ---------------------
from ._expr import Expr, col, dst, id, lit, src

# --- Frame types ------------------------------------------------------------
from ._frames import EdgeFrame, NodeFrame

# --- Graph verbs & algorithms ----------------------------------------------
from ._graph import (
    betweenness,
    closeness,
    clustering_coefficient,
    connected_components,
    degree,
    hop,
    label_propagation,
    louvain,
    neighbors,
    pagerank,
    random_walk,
    shortest_path,
    triangle_count,
)

# --- IO constructors --------------------------------------------------------
from ._io import (
    from_arrow,
    from_polars,
    read_edges,
    read_nodes,
    scan_edges,
    scan_nodes,
)
from ._result import MaterializedFrame

# --- Graph-level statistics -------------------------------------------------
from ._stats import avg_path_length, density, describe, diameter

# --- Native extension (optional until built) --------------------------------
_native: ModuleType | None = None
try:
    from . import _ursa

    _native = _ursa
    __core_version__ = _ursa.__core_version()
    from . import demo  # noqa: F401  (thin wrappers over the native demo kernels)

    _NATIVE_AVAILABLE = True
except ImportError:  # pragma: no cover - native module not yet built
    __core_version__ = None
    _NATIVE_AVAILABLE = False

# Single-sourced from the installed distribution metadata (which maturin fills
# from the Cargo workspace version), so it never drifts from __core_version__.
try:
    __version__ = version("ursa-graph")
except PackageNotFoundError:  # source checkout without an install
    __version__ = "0.0.0+dev"

__all__ = [
    "EdgeFrame",
    "Expr",
    "MaterializedFrame",
    "NodeFrame",
    "avg_path_length",
    "betweenness",
    "closeness",
    "clustering_coefficient",
    "col",
    "connected_components",
    "degree",
    "density",
    "describe",
    "diameter",
    "dst",
    "from_arrow",
    "from_polars",
    "hop",
    "id",
    "label_propagation",
    "lit",
    "louvain",
    "neighbors",
    "pagerank",
    "random_walk",
    "read_edges",
    "read_nodes",
    "scan_edges",
    "scan_nodes",
    "shortest_path",
    "src",
    "triangle_count",
]
