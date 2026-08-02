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

The primary surface is live: the algorithms (pagerank, degree, connected
components, triangle count, clustering, closeness, betweenness, label propagation,
louvain — with weighted variants), neighbour aggregation, the traversals (hop,
shortest_path, random_walk), the whole-graph stats (density, avg_path_length,
diameter, describe), and both in-memory sources (the ``EdgeFrame(data, src=, dst=)``
/ ``NodeFrame(data, id=)`` constructors, taking row dicts, a column dict, a polars
or pandas DataFrame, or pyarrow — with ``from_arrow``/``from_polars``/``from_pandas``
as typed aliases) and scan-backed ones (``scan_edges``/``scan_nodes``, Parquet/CSV,
local or object storage) all execute end to end through the DataFusion plan and the
``ursa-core`` kernels. However a frame is built, it behaves identically thereafter.
A few relational verbs are still plan-only and raise a clear error when collected
(``sample``, ``group_by().agg``, ``join``, ``schema``); ``ur.col`` & the expression
dialect are pure Python and always available.
"""

from __future__ import annotations

from importlib.metadata import PackageNotFoundError, version
from types import ModuleType

# --- Public datasets (bundled + downloadable) -------------------------------
from . import datasets

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
from ._interop import (
    from_networkx,
    from_numpy,
    from_scipy_sparse,
    nodes_from_networkx,
)
from ._io import (
    from_arrow,
    from_edgelist,
    from_pandas,
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

    # The exception hierarchy the native engine raises. Re-exported so user code
    # can `except ursa.UrsaError` (or the narrower subclasses) regardless of
    # whether the extension is built.
    from ._ursa import ColumnNotFoundError, ComputeError, UrsaError

    _NATIVE_AVAILABLE = True
except ImportError:  # pragma: no cover - native module not yet built
    __core_version__ = None
    _NATIVE_AVAILABLE = False

    # Pure-Python stand-ins so `ursa.UrsaError` exists (and `except ursa.UrsaError`
    # is valid) even without the compiled engine. Once the extension is built the
    # native classes above replace these; identity does not matter because only
    # the native path ever raises them.
    class UrsaError(Exception):
        """Base class for every error Ursa raises from native execution."""

    class ColumnNotFoundError(UrsaError):
        """A query referenced a column that does not exist in the frame."""

    class ComputeError(UrsaError):
        """A graph computation failed (invalid parameter, unsupported op, kernel error)."""


# Single-sourced from the installed distribution metadata (which maturin fills
# from the Cargo workspace version), so it never drifts from __core_version__.
try:
    __version__ = version("ursa-graph")
except PackageNotFoundError:  # source checkout without an install
    __version__ = "0.0.0+dev"

__all__ = [
    "ColumnNotFoundError",
    "ComputeError",
    "EdgeFrame",
    "Expr",
    "MaterializedFrame",
    "NodeFrame",
    "UrsaError",
    "avg_path_length",
    "betweenness",
    "closeness",
    "clustering_coefficient",
    "col",
    "connected_components",
    "datasets",
    "degree",
    "density",
    "describe",
    "diameter",
    "dst",
    "from_arrow",
    "from_edgelist",
    "from_networkx",
    "from_numpy",
    "from_pandas",
    "from_polars",
    "from_scipy_sparse",
    "hop",
    "id",
    "label_propagation",
    "lit",
    "louvain",
    "neighbors",
    "nodes_from_networkx",
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
