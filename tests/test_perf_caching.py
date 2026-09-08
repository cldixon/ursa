"""Index/attribute caching (perf #47): one CSR build per graph, shared across a
frame and its property-preserving derivations via a per-frame build cell (with its
own lock, so unrelated frames build concurrently rather than serializing).

The native ``_topology_build_count()`` hook makes the index-build count
observable.
"""

import threading

import pyarrow as pa
import pytest

import ursa as ur
from ursa._execute import _native

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges():
    return ur.from_arrow(pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0]}), src="s", dst="d")


def test_repeated_collects_over_one_frame_build_the_csr_once():
    edges = _edges()
    before = _native()._topology_build_count()
    ur.pagerank(edges).collect()
    ur.degree(edges).collect()
    ur.closeness(edges).collect()
    assert _native()._topology_build_count() - before == 1


def test_property_preserving_derivation_shares_the_index_cell():
    # #47.3: a property-only derivation shares the *same* index cell object as its
    # parent (by reference, not a snapshot), so a build via either is visible to
    # the other instead of each rebuilding an identical CSR.
    edges = _edges()
    derived = edges.select("s", "d")  # property-only: index preserved
    assert derived._index_build_cell is edges._index_build_cell
    # Each frame carries its own lock on that shared cell.
    assert derived._index_build_cell.lock is edges._index_build_cell.lock


def test_structural_op_gets_a_fresh_index_cell():
    # A row-reshaping op (distinct/sample/join/group_by) drops the index: the derived
    # frame gets its own fresh cell, so a stale CSR is never carried across. (filter is
    # NOT such an op anymore — it is a subgraph view that shares the parent cell; see
    # test_subgraph_filter_shares_the_parent_index_cell.)
    edges = _edges()
    reshaped = edges.distinct()
    assert reshaped._index_build_cell is not edges._index_build_cell


def test_subgraph_filter_shares_the_parent_index_cell():
    # #114: filter is a subgraph view over the parent topology, so it shares the
    # parent's index cell (no fresh CSR) — a build via either frame is visible to the
    # other, and a graph op on the filtered frame reuses the parent CSR.
    edges = _edges()
    viewed = edges.filter(ur.col("s") >= 0)
    assert viewed._index_build_cell is edges._index_build_cell


def test_distinct_graphs_build_separate_indexes():
    a = ur.from_arrow(pa.table({"s": [0, 1], "d": [1, 0]}), src="s", dst="d")
    b = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    before = _native()._topology_build_count()
    ur.degree(a).collect()
    ur.degree(b).collect()
    assert _native()._topology_build_count() - before == 2


def test_concurrent_first_collects_build_once_and_agree():
    # #53.3: N threads racing to first-collect one fresh frame must build the CSR
    # exactly once (the double-checked per-cell lock) and all agree — the
    # concurrency guarantee behind the index-preservation contract, GIL released.
    edges = _edges()
    n_threads = 8
    before = _native()._topology_build_count()
    results: list = [None] * n_threads
    barrier = threading.Barrier(n_threads)

    def worker(i: int) -> None:
        barrier.wait()  # release all threads together to maximize the race
        results[i] = {r["id"]: r["pagerank"] for r in ur.pagerank(edges).collect().to_dicts()}

    threads = [threading.Thread(target=worker, args=(i,)) for i in range(n_threads)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()

    assert _native()._topology_build_count() - before == 1
    assert all(r == results[0] for r in results[1:])
