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
    nodes: Any | None = ...,
    nodes_id: str | None = ...,
) -> Any: ...

# A hop traversal: edge arrays + int64 seed array + n/direction + relational
# tail -> (src, dst) pyarrow.RecordBatch of reached pairs.
def run_hop_query(
    src: Any,
    dst: Any,
    seeds: Any,
    n: int,
    direction: str,
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None = ...,
    limit: int | None = ...,
    distinct: bool = ...,
) -> Any: ...

# A shortest_path traversal: edge arrays + int64 source/target + direction +
# relational tail -> (src, dst, hop) pyarrow.RecordBatch of the path edges.
def run_path_query(
    src: Any,
    dst: Any,
    source: int,
    target: int,
    direction: str,
    weighted: bool = ...,
    filters: list[tuple[str, str, float]] = ...,
    sort: tuple[str, bool] | None = ...,
    limit: int | None = ...,
    distinct: bool = ...,
) -> Any: ...

# Whole-graph directed edge density (eager scalar).
def graph_density(src: Any, dst: Any) -> float: ...

# Average shortest-path length over reachable ordered pairs (eager scalar).
def graph_avg_path_length(src: Any, dst: Any, sample: float | None = ...) -> float: ...

# Graph diameter (eager scalar); approximate is a lower-bound estimate.
def graph_diameter(src: Any, dst: Any, approximate: bool) -> int: ...

# Whole-graph one-row summary -> pyarrow.RecordBatch.
def graph_describe(src: Any, dst: Any, full: bool) -> Any: ...

# Scan a Parquet/CSV edge file (local or s3://gs://az://file://) -> (src, dst)
# pyarrow.RecordBatch. storage_options seeds the object-store backend.
def scan_edges_arrow(
    path: str, src: str, dst: str, storage_options: dict[str, str] | None = ...
) -> Any: ...

# Scan a Parquet/CSV node file -> full attribute pyarrow.RecordBatch (id cast int64).
def scan_nodes_arrow(path: str, id: str, storage_options: dict[str, str] | None = ...) -> Any: ...

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
