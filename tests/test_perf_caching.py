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


# --- subgraph-view edge mask memo (#175, cached in Rust since #192) -----------
# Masks are cached on the native index, keyed by the view (its filters, or its
# traversal and seeds); `_mask_build_count()` counts the masks an index computed.
def _mask_builds(edges):
    """Masks computed over ``edges``' index so far (0 before it is built)."""
    index = edges._index_build_cell.value
    return 0 if index is None else index._mask_build_count()


def _weighted():
    return pa.table(
        {"s": [0, 0, 1, 2, 3, 3], "d": [1, 2, 0, 0, 1, 2], "w": [0.1, 0.9, 0.6, 0.2, 0.7, 0.8]}
    )


def test_filtered_view_computes_its_mask_once_across_collects():
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    first = ur.degree(view).collect().to_arrow()
    assert ur.degree(view).collect().to_arrow().equals(first)
    assert ur.pagerank(view).collect().to_arrow().num_rows == first.num_rows
    assert _mask_builds(edges) == 1
    # Byte-identical to a fresh frame with no memo.
    fresh = ur.degree(ur.from_arrow(_weighted(), src="s", dst="d").filter(ur.col("w") > 0.5))
    assert fresh.collect().to_arrow().equals(first)


def test_scanned_view_computes_its_mask_once_across_collects(tmp_path):
    import pyarrow.parquet as pq

    path = tmp_path / "edges.parquet"
    pq.write_table(_weighted(), path)
    edges = ur.scan_edges(str(path), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    _native()._scan_log(True)
    first = ur.degree(view).collect().to_arrow()
    assert ur.degree(view).collect().to_arrow().equals(first)
    # One mask, from one read of the edge rows; the repeat reads nothing.
    assert _mask_builds(edges) == 1
    assert [kind for kind, _, _ in _native()._scan_log(False)] == ["index", "table"]
    in_memory = ur.from_arrow(_weighted(), src="s", dst="d").filter(ur.col("w") > 0.5)
    assert ur.degree(in_memory).collect().to_arrow().equals(first)


def test_chained_filter_gets_its_own_mask_and_intersects():
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    ur.degree(view).collect()
    narrower = view.filter(ur.src() != 3)
    got = ur.degree(narrower).collect().to_arrow()
    ur.degree(narrower).collect()
    assert _mask_builds(edges) == 2  # one per view, none on repeats
    both = edges.filter((ur.col("w") > 0.5) & (ur.src() != 3))
    assert ur.degree(both).collect().to_arrow().equals(got)
    # The parent's memo was not clobbered by the narrower view.
    assert (
        ur.degree(view)
        .collect()
        .to_arrow()
        .equals(ur.degree(edges.filter(ur.col("w") > 0.5)).collect().to_arrow())
    )


def test_traversal_view_computes_its_mask_once_across_collects():
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    for view in (ur.hop(edges, n=1).from_([3]), ur.shortest_path(edges, 3, 0)):
        ur.degree(edges).collect()
        before = _mask_builds(edges)
        first = ur.degree(view).collect().to_arrow()
        assert ur.degree(view).collect().to_arrow().equals(first)
        assert _mask_builds(edges) - before == 1


def test_cached_mask_is_not_served_for_a_different_index():
    # The memo lives on the parent index object: if the parent CSR is rebuilt
    # (a weighted scan replaces it), the mask is recomputed against the new one.
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    first = ur.degree(view).collect().to_arrow()
    assert _mask_builds(edges) == 1
    cell = edges._index_build_cell
    with cell.lock:
        cell.value = _native().build_index(_weighted().select(["s", "d"]).to_batches())
    assert ur.degree(view).collect().to_arrow().equals(first)
    assert _mask_builds(edges) == 1  # computed afresh over the new index


def test_concurrent_collects_on_a_view_compute_the_mask_once():
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    view = edges.filter(ur.col("w") > 0.5)
    results = []
    threads = [
        threading.Thread(target=lambda: results.append(ur.degree(view).collect().to_arrow()))
        for _ in range(8)
    ]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert _mask_builds(edges) == 1
    assert all(r.equals(results[0]) for r in results)


# --- weighted queries reuse the index and the evaluated weights (#175) -------
def _weighted_table():
    return pa.table(
        {
            "s": [0, 0, 1, 2, 3, 3, 4],
            "d": [1, 2, 0, 0, 4, 0, 1],
            "w": [1.0, 9.0, 1.0, 2.0, 0.5, 3.0, 7.0],
            "fx": [2.0, 1.0, 1.0, 0.5, 4.0, 1.0, 1.0],
        }
    )


def _weighted_pr(edges, weight):
    return (
        edges.nodes()
        .with_columns(pr=ur.pagerank(edges, weight=weight), deg=ur.degree(edges))
        .collect()
        .to_arrow()
    )


@pytest.fixture(params=["memory", "scan"])
def weighted_source(request, tmp_path):
    """A factory for fresh frames over the same weighted edges, in memory or scanned."""
    if request.param == "memory":
        return lambda: ur.from_arrow(_weighted_table(), src="s", dst="d")
    import pyarrow.parquet as pq

    path = tmp_path / "w.parquet"
    pq.write_table(_weighted_table(), path)
    return lambda: ur.scan_edges(str(path), src="s", dst="d")


def test_weighted_collects_build_the_index_once_and_evaluate_weights_once(weighted_source):
    edges = weighted_source()
    before = _native()._topology_build_count()
    first = _weighted_pr(edges, ur.col("w"))
    index = edges._index_build_cell.value
    assert index._weight_eval_count() == 1
    for _ in range(3):
        assert _weighted_pr(edges, ur.col("w")).equals(first)
    # One CSR build, kept across collects (not rebuilt by each weighted scan), and
    # one weight evaluation served from the index's cache thereafter.
    assert _native()._topology_build_count() - before == 1
    assert edges._index_build_cell.value is index
    assert index._weight_eval_count() == 1
    # A different expression is evaluated (once) on top of the first.
    _weighted_pr(edges, ur.col("w") * ur.col("fx"))
    _weighted_pr(edges, ur.col("w") * ur.col("fx"))
    assert index._weight_eval_count() == 2
    assert _native()._topology_build_count() - before == 1


def test_weighted_results_match_a_fresh_frame(weighted_source):
    # Cached weights (and an index built by an earlier unweighted collect, natively
    # for a scan) give results identical to a fresh frame's first weighted collect.
    edges = weighted_source()
    ur.degree(edges).collect()  # index built before any weights are read
    for weight in (ur.col("w"), ur.col("w") * ur.col("fx"), ur.col("w")):
        got = _weighted_pr(edges, weight)
        assert got.equals(_weighted_pr(weighted_source(), weight))


def test_weighted_scan_reuses_index_after_on_null_drop(tmp_path):
    # Under on_null="drop" the weight rows must stay aligned with the index's kept
    # rows when the index came from an earlier (native, unweighted) build.
    import pyarrow.parquet as pq

    t = pa.table(
        {"s": [0, None, 1, 2, 3], "d": [1, 2, 0, None, 0], "w": [1.0, 50.0, 4.0, 60.0, 2.0]}
    )
    path = tmp_path / "w.parquet"
    pq.write_table(t, path)
    clean = ur.from_arrow(t.take([0, 2, 4]), src="s", dst="d")  # the non-null rows
    expected = _weighted_pr(clean, ur.col("w"))
    edges = ur.scan_edges(str(path), src="s", dst="d", on_null="drop")
    with pytest.warns(UserWarning, match="dropped 2 edge row"):
        ur.degree(edges).collect()
    before = _native()._topology_build_count()
    with pytest.warns(UserWarning, match="dropped 2 edge row"):
        assert _weighted_pr(edges, ur.col("w")).equals(expected)
    assert _weighted_pr(edges, ur.col("w")).equals(expected)  # cached: no rescan
    assert _native()._topology_build_count() == before


def test_weighted_shortest_path_over_a_scan_keeps_the_index(tmp_path):
    import pyarrow.parquet as pq

    path = tmp_path / "w.parquet"
    pq.write_table(_weighted_table(), path)
    edges = ur.scan_edges(str(path), src="s", dst="d")
    ur.degree(edges).collect()
    index = edges._index_build_cell.value
    out = ur.shortest_path(edges, 3, 2, weight=ur.col("w")).collect().to_arrow()
    assert edges._index_build_cell.value is index
    fresh = ur.shortest_path(
        ur.scan_edges(str(path), src="s", dst="d"), 3, 2, weight=ur.col("w")
    ).collect()
    assert out.equals(fresh.to_arrow())


# --- reshaped edge sets: the subplan-fingerprint topology cache (#191) ---------
# A reshaped edge frame has no index cell of its own; its index is built from its
# rows and cached in the engine under a fingerprint of the edge plan, so a repeat
# does not rebuild it, while a different plan or a changed file never shares it.
def test_a_reshaped_frame_builds_its_index_once_across_collects():
    edges = ur.from_arrow(_weighted(), src="s", dst="d")
    grouped = edges.group_by("s", "d").agg(w=ur.col("w").sum())
    first = ur.pagerank(grouped, weight=ur.col("w")).collect().to_arrow()
    before = _native()._topology_build_count()
    # The same plan written again (new frame objects) hits the cache too.
    again = edges.group_by("s", "d").agg(w=ur.col("w").sum())
    assert ur.pagerank(again, weight=ur.col("w")).collect().to_arrow().equals(first)
    ur.degree(grouped).collect()
    assert _native()._topology_build_count() == before
    # A different subplan is a different topology.
    ur.degree(edges.group_by("s", "d").agg(w=ur.col("w").max())).collect()
    ur.degree(edges.distinct()).collect()
    assert _native()._topology_build_count() == before + 2
    # So is the same plan over another table with the same rows.
    other = ur.from_arrow(_weighted(), src="s", dst="d")
    ur.degree(other.group_by("s", "d").agg(w=ur.col("w").sum())).collect()
    assert _native()._topology_build_count() == before + 3


def test_a_reshaped_scan_rebuilds_when_its_file_changes(tmp_path):
    import os

    import pyarrow.parquet as pq

    path = tmp_path / "edges.parquet"
    pq.write_table(_weighted(), path)
    edges = ur.scan_edges(str(path), src="s", dst="d")
    first = ur.degree(edges.distinct()).collect().to_arrow()
    before = _native()._topology_build_count()
    assert ur.degree(edges.distinct()).collect().to_arrow().equals(first)
    assert _native()._topology_build_count() == before
    # Rewritten in place (same size, a later time): read and built afresh.
    changed = _weighted().set_column(1, "d", pa.array([2, 1, 0, 0, 1, 2]))
    pq.write_table(changed, path)
    st = os.stat(path)
    os.utime(path, ns=(st.st_atime_ns, st.st_mtime_ns + 1_000_000))
    got = ur.degree(edges.distinct()).collect().to_arrow()
    assert _native()._topology_build_count() == before + 1
    fresh = ur.degree(ur.from_arrow(changed, src="s", dst="d").distinct()).collect().to_arrow()
    assert got.equals(fresh)
