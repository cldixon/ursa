"""Index/attribute caching (perf #47): one CSR build per graph, shared across a
frame and its property-preserving derivations via a per-frame build cell (with its
own lock, so unrelated frames build concurrently rather than serializing).

The native ``_topology_build_count()`` hook makes the index-build count
observable.
"""

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
    # A row-changing op (filter) drops the index: the derived frame gets its own
    # fresh cell, so a stale CSR is never carried across.
    edges = _edges()
    filtered = edges.filter(ur.col("s") >= 0)
    assert filtered._index_build_cell is not edges._index_build_cell


def test_distinct_graphs_build_separate_indexes():
    a = ur.from_arrow(pa.table({"s": [0, 1], "d": [1, 0]}), src="s", dst="d")
    b = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    before = _native()._topology_build_count()
    ur.degree(a).collect()
    ur.degree(b).collect()
    assert _native()._topology_build_count() - before == 2
