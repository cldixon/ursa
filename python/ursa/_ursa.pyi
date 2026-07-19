"""Type stub for the maturin-built native extension ``ursa._ursa``.

The compiled module has no Python source for the type checker to read, so this
stub declares its surface. Keep it in sync with ``ursa-py/src/lib.rs``.
"""

from typing import Any

def __core_version() -> str: ...

# Real execution path: pyarrow int64 arrays in, pyarrow.RecordBatch out.
def run_pagerank(
    src: Any,
    dst: Any,
    damping: float = ...,
    max_iter: int = ...,
    tol: float = ...,
) -> Any: ...
def run_degree(src: Any, dst: Any, direction: str = ...) -> Any: ...
def run_connected_components(src: Any, dst: Any) -> Any: ...
def run_triangle_count(src: Any, dst: Any) -> Any: ...

# Demo path: plain lists in and out (pure Python->Rust smoke tests).
def _demo_pagerank(
    src: list[int],
    dst: list[int],
    damping: float = ...,
    max_iter: int = ...,
    tol: float = ...,
) -> list[tuple[int, float]]: ...
def _demo_pagerank(
    src: list[int],
    dst: list[int],
    damping: float = ...,
    max_iter: int = ...,
    tol: float = ...,
) -> list[tuple[int, float]]: ...
def _demo_degree(
    src: list[int],
    dst: list[int],
    direction: str = ...,
) -> list[tuple[int, int]]: ...
def _demo_connected_components(
    src: list[int],
    dst: list[int],
) -> list[tuple[int, int]]: ...
