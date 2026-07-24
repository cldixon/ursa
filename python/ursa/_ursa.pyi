"""Type stub for the maturin-built native extension ``ursa._ursa``.

The compiled module has no Python source for the type checker to read, so this
stub declares its surface. Keep it in sync with ``ursa-py/src/lib.rs``.
"""

from typing import Any

# The exception hierarchy (defined in Rust via create_exception!). Every engine
# error reaches Python as one of these — ordinary exceptions, so `except Exception`
# catches them (a Rust panic would escape as a BaseException instead).
class UrsaError(Exception): ...
class ColumnNotFoundError(UrsaError): ...
class ComputeError(UrsaError): ...

def __core_version() -> str: ...

# The CSR topology + IdMap, built once and cached on the EdgeFrame. Opaque handle;
# built by build_index and passed to every graph op over the frame.
class GraphIndex: ...

# Build the graph index from (src, dst) id arrays (int64 or string; the one CSR
# build site — the id type is auto-detected from the Arrow columns).
def build_index(src: Any, dst: Any) -> GraphIndex: ...

# Total topology builds so far (test/observability hook for the index contract).
def _topology_build_count() -> int: ...

# The one execution entry point: a built index + a JSON column IR +
# filter/sort/limit -> pyarrow.RecordBatch.
def run_node_query(
    index: GraphIndex,
    columns_json: str,
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None = ...,
    limit: int | None = ...,
    nodes: Any | None = ...,
    nodes_id: str | None = ...,
    edges: Any | None = ...,
) -> Any: ...

# A hop traversal: a built index + user-id seed array (int64 or string) +
# n/direction + relational
# tail -> (src, dst) pyarrow.RecordBatch of reached pairs.
def run_hop_query(
    index: GraphIndex,
    seeds: Any,
    n: int,
    direction: str,
    filters: list[tuple[str, str, float]],
    sort: tuple[str, bool] | None = ...,
    limit: int | None = ...,
    distinct: bool = ...,
) -> Any: ...

# A shortest_path traversal: a built index + 1-element source/target user-id
# arrays (int64 or string) + direction + relational tail -> (src, dst, hop)
# pyarrow.RecordBatch of the path edges.
def run_path_query(
    index: GraphIndex,
    source: Any,
    target: Any,
    direction: str,
    weight: str | None = ...,
    edges: Any | None = ...,
    filters: list[tuple[str, str, float]] = ...,
    sort: tuple[str, bool] | None = ...,
    limit: int | None = ...,
    distinct: bool = ...,
) -> Any: ...

# A random_walk: a built index + user-id start ids (int64 or string) + walk params
# + relational tail
# -> (walk_id, step, node) pyarrow.RecordBatch of the walk rows.
def run_walk_query(
    index: GraphIndex,
    starts: Any,
    steps: int,
    walks_per_node: int,
    seed: int | None = ...,
    filters: list[tuple[str, str, float]] = ...,
    sort: tuple[str, bool] | None = ...,
    limit: int | None = ...,
    distinct: bool = ...,
) -> Any: ...

# Whole-graph directed edge density (eager scalar).
def graph_density(index: GraphIndex) -> float: ...

# Average shortest-path length over reachable ordered pairs (eager scalar).
def graph_avg_path_length(index: GraphIndex, sample: float | None = ...) -> float: ...

# Graph diameter (eager scalar); approximate is a lower-bound estimate.
def graph_diameter(index: GraphIndex, approximate: bool) -> int: ...

# Whole-graph one-row summary -> pyarrow.RecordBatch.
def graph_describe(index: GraphIndex, full: bool) -> Any: ...

# Scan a Parquet/CSV edge file (local or s3://gs://az://file://) -> (src, dst)
# pyarrow.RecordBatch. storage_options seeds the object-store backend.
def scan_edges_arrow(
    path: str,
    src: str,
    dst: str,
    storage_options: dict[str, str] | None = ...,
    weight_columns: list[str] = ...,
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
