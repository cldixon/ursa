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
class GraphIndex:
    def has_weights(self, weight_json: str) -> bool: ...
    def _weight_eval_count(self) -> int: ...
    def _mask_build_count(self) -> int: ...

# A scan_nodes frame's scanned table, kept in the frame's cell: one superset of
# the columns read so far. Opaque handle; the plan reads through it.
class NodeTableCache:
    def __init__(self) -> None: ...
    def _scan_count(self) -> int: ...
    def _held(self) -> list[str] | None: ...

# Build the graph index from an edge batch list — a list of pyarrow RecordBatches
# whose first two columns are the (canonicalized) src/dst id columns (int64 or
# string; the one CSR build site). The batches are interned as a stream, so a large
# edge table never has to be concatenated into one contiguous batch first.
def build_index(edges: Any) -> GraphIndex: ...

# Total topology builds so far (test/observability hook for the index contract).
def _topology_build_count() -> int: ...

# Turn the scan log on (clearing it) or off; returns (kind, columns, filters
# pushed) per native scan since it was last turned on (test/observability hook).
def _scan_log(record: bool) -> list[tuple[str, list[str], int]]: ...

# Scan an edge file and build its index in one call -> (index, rows dropped).
def scan_build_index(
    path: str,
    src: str,
    dst: str,
    storage_options: dict[str, str] | None = ...,
    drop_nulls: bool = ...,
) -> tuple[GraphIndex, int]: ...

# The one execution entry point: a serialized plan (ursa_plan::plan::Plan as JSON)
# and the host that resolves its slots (ursa._execute._Host) ->
# list[pyarrow.RecordBatch] (a chunked result).
def run_plan(plan: str, host: Any) -> Any: ...

# The checked graph index of a serialized edge input (ursa_plan::plan::EdgeInput),
# for the eager whole-graph stats.
def resolve_index(edges: str, host: Any) -> GraphIndex: ...

# The whole-graph stats take direction "out" (default), "in" or "both". "both"
# reads the graph as undirected: one edge per distinct unordered pair, self-loops
# dropped (NetworkX's G.to_undirected()). "in" equals "out" over the whole graph.

# Whole-graph edge density (eager scalar); "both" is the undirected density.
def graph_density(index: GraphIndex, direction: str = ...) -> float: ...

# Average shortest-path length over reachable pairs (eager scalar).
def graph_avg_path_length(
    index: GraphIndex, sample: float | None = ..., direction: str = ...
) -> float: ...

# Graph diameter (eager scalar); approximate is a lower-bound estimate.
def graph_diameter(index: GraphIndex, approximate: bool, direction: str = ...) -> int: ...

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
