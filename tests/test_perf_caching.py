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


# --- subgraph-view edge mask memo (#175) -------------------------------------
def _count_mask_builds(monkeypatch):
    """Wrap the uncached mask evaluation so each (re)computation is counted."""
    import ursa._execute as ex

    calls = []
    real = ex._compute_edge_mask

    def counting(*args, **kwargs):
        calls.append(args[0])
        return real(*args, **kwargs)

    monkeypatch.setattr(ex, "_compute_edge_mask", counting)
    return calls


def _weighted():
    return pa.table(
        {"s": [0, 0, 1, 2, 3, 3], "d": [1, 2, 0, 0, 1, 2], "w": [0.1, 0.9, 0.6, 0.2, 0.7, 0.8]}
    )


def test_filtered_view_computes_its_mask_once_across_collects(monkeypatch):
    calls = _count_mask_builds(monkeypatch)
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    first = ur.degree(view).collect().to_arrow()
    assert ur.degree(view).collect().to_arrow().equals(first)
    assert ur.pagerank(view).collect().to_arrow().num_rows == first.num_rows
    assert len(calls) == 1
    # Byte-identical to a fresh frame with no memo.
    fresh = ur.degree(ur.from_arrow(_weighted(), src="s", dst="d").filter(ur.col("w") > 0.5))
    assert fresh.collect().to_arrow().equals(first)


def test_scanned_view_computes_its_mask_once_across_collects(monkeypatch, tmp_path):
    import pyarrow.parquet as pq

    path = tmp_path / "edges.parquet"
    pq.write_table(_weighted(), path)
    calls = _count_mask_builds(monkeypatch)
    view = ur.scan_edges(str(path), src="s", dst="d").filter(ur.col("w") > 0.5)
    first = ur.degree(view).collect().to_arrow()
    assert ur.degree(view).collect().to_arrow().equals(first)
    assert len(calls) == 1
    in_memory = ur.from_arrow(_weighted(), src="s", dst="d").filter(ur.col("w") > 0.5)
    assert ur.degree(in_memory).collect().to_arrow().equals(first)


def test_chained_filter_gets_its_own_mask_and_intersects(monkeypatch):
    calls = _count_mask_builds(monkeypatch)
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    ur.degree(view).collect()
    narrower = view.filter(ur.src() != 3)
    assert narrower._mask_build_cell is not view._mask_build_cell
    got = ur.degree(narrower).collect().to_arrow()
    ur.degree(narrower).collect()
    assert len(calls) == 2  # one per frame, none on repeats
    both = edges.filter((ur.col("w") > 0.5) & (ur.src() != 3))
    assert ur.degree(both).collect().to_arrow().equals(got)
    # The parent's memo was not clobbered by the narrower view.
    assert (
        ur.degree(view)
        .collect()
        .to_arrow()
        .equals(ur.degree(edges.filter(ur.col("w") > 0.5)).collect().to_arrow())
    )


def test_traversal_view_computes_its_mask_once_across_collects(monkeypatch):
    calls = _count_mask_builds(monkeypatch)
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    for view in (ur.hop(edges, n=1).from_([3]), ur.shortest_path(edges, 3, 0)):
        calls.clear()
        first = ur.degree(view).collect().to_arrow()
        assert ur.degree(view).collect().to_arrow().equals(first)
        assert len(calls) == 1


def test_cached_mask_is_not_served_for_a_different_index(monkeypatch):
    # The memo is keyed on the parent index object: if the parent CSR is rebuilt
    # (a weighted scan replaces it), the mask is recomputed against the new one.
    calls = _count_mask_builds(monkeypatch)
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    first = ur.degree(view).collect().to_arrow()
    cell = edges._index_build_cell
    with cell.lock:
        cell.value = _native().build_index(_weighted().select(["s", "d"]).to_batches())
    assert ur.degree(view).collect().to_arrow().equals(first)
    assert len(calls) == 2


def test_concurrent_collects_on_a_view_compute_the_mask_once(monkeypatch):
    calls = _count_mask_builds(monkeypatch)
    view = ur.from_arrow(_weighted(), src="s", dst="d").filter(ur.col("w") > 0.5)
    results = []
    threads = [
        threading.Thread(target=lambda: results.append(ur.degree(view).collect().to_arrow()))
        for _ in range(8)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert len(calls) == 1
    assert all(r.equals(results[0]) for r in results)
