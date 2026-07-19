"""Executing a node-valued graph expression — the walking-skeleton ``collect()``.

This is the thin Python orchestration over the real Rust execution path
(``ursa_plan::GraphAlgorithmExec`` behind ``ursa._ursa.run_*``). It handles
exactly the slice that is wired end-to-end today: a standalone node-valued
algorithm (``ur.pagerank(edges)``, ``ur.degree(edges)``, ...) over an EdgeFrame
that has an in-memory Arrow source (``from_arrow`` / ``from_polars``).

Everything outside that slice — collecting a composed ``with_columns`` pipeline,
or executing over a ``scan_*`` file source — raises a clear, honest error rather
than pretending. Those are the next increments.
"""

from __future__ import annotations

from typing import TYPE_CHECKING, Any

from ._result import MaterializedFrame

if TYPE_CHECKING:
    from ._expr import Expr

# Algorithms with a kernel wired into the execution path (mirrors
# ursa_plan::result::is_executable on the Rust side).
_EXECUTABLE = {"pagerank", "degree", "connected_components", "triangle_count"}


def collect_graph_expr(expr: Expr) -> MaterializedFrame:
    """Execute a standalone node-valued graph expression and materialize it."""
    if expr.kind != "graph":
        raise NotImplementedError(
            "collect() on this skeleton is wired only for standalone node-valued "
            "graph algorithms (e.g. ur.pagerank(edges).collect()); composed "
            "pipelines and non-graph expressions are not executable yet."
        )

    verb = expr.payload["verb"]
    if verb not in _EXECUTABLE:
        raise NotImplementedError(
            f"the '{verb}' kernel is not wired into the execution path yet "
            f"(wired: {', '.join(sorted(_EXECUTABLE))})."
        )

    edges = expr.payload.get("edges")
    arrays = getattr(edges, "_edge_arrays", None) if edges is not None else None
    if arrays is None:
        raise NotImplementedError(
            "collect() needs in-memory edges built via ur.from_arrow(...) or "
            "ur.from_polars(...); executing over a scan_* file source is not "
            "wired into collect() yet."
        )

    native = _native()
    src, dst = arrays
    p = expr.payload
    if verb == "pagerank":
        batch = native.run_pagerank(
            src, dst, p.get("damping", 0.85), p.get("max_iter", 30), p.get("tol", 1e-6)
        )
    elif verb == "degree":
        batch = native.run_degree(src, dst, p.get("direction", "out"))
    elif verb == "connected_components":
        batch = native.run_connected_components(src, dst)
    else:  # triangle_count
        batch = native.run_triangle_count(src, dst)

    return MaterializedFrame(batch)


def _native() -> Any:
    try:
        from . import _ursa
    except ImportError as exc:  # pragma: no cover - native module not built
        raise RuntimeError(
            "the native extension (ursa._ursa) is not built; run `uv sync` or "
            "`maturin develop` to enable collect()."
        ) from exc
    return _ursa
