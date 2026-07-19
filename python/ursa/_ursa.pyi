"""Type stub for the maturin-built native extension ``ursa._ursa``.

The compiled module has no Python source for the type checker to read, so this
stub declares its surface. Keep it in sync with ``ursa-py/src/lib.rs``.
"""

from typing import Any

def __core_version() -> str: ...

# The one execution entry point: pyarrow edge arrays + a JSON column IR +
# filter/sort/limit -> pyarrow.RecordBatch.
def run_node_query(
    src: Any,
    dst: Any,
    columns_json: str,
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None = ...,
    limit: int | None = ...,
) -> Any: ...

# Scan a Parquet/CSV edge file -> (src, dst) pyarrow.RecordBatch.
def scan_edges_arrow(path: str, src: str, dst: str) -> Any: ...

# Demo path: plain lists in and out (pure Python->Rust smoke tests).
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
