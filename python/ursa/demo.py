"""Thin, honest wrappers over the native *demo* kernels.

These are **not** the public API — they are the end-to-end proof that the
Python → PyO3 → ursa-core path works with the *real* Rust kernels (CSR build,
pull-based PageRank, union-find components, degree) over in-memory edge lists.
They exist so the wiring is exercisable and testable today, before the lazy
DataFrame/DataFusion surface is built. Expect them to be removed once
``EdgeFrame.collect()`` is real.

    from ursa import demo
    demo.pagerank([0, 0, 1, 2], [1, 2, 2, 0])   # -> [(node, score), ...]
"""

from __future__ import annotations

from . import _ursa


def pagerank(src, dst, *, damping: float = 0.85, max_iter: int = 30, tol: float = 1e-6):
    """Real pull-based PageRank over a dense edge list. Returns (node, score) pairs."""
    return _ursa._demo_pagerank(list(src), list(dst), damping, max_iter, tol)


def degree(src, dst, *, direction: str = "out"):
    """Real degree kernel. ``direction`` is 'out' | 'in' | 'both'."""
    return _ursa._demo_degree(list(src), list(dst), direction)


def connected_components(src, dst):
    """Real weak-connected-components (union-find). Returns (node, label) pairs."""
    return _ursa._demo_connected_components(list(src), list(dst))
