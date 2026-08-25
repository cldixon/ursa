"""Graph ops over a traversal result (#116): a node-valued kernel over a
``hop`` / ``shortest_path`` frame.

A traversal produces a **reached node set** (the seeds' k-hop region for ``hop``;
the path's nodes for ``shortest_path``, seeds included). A node-valued kernel over
that frame runs over the **induced subgraph** of the reached nodes — keep a parent
edge iff *both* endpoints are reached — expressed as a #114 edge mask over the
parent CSR, so there is no rebuild and the parent id space is preserved. A node
reached-but-isolated-by-the-mask stays present at degree 0.

This reuses #114's masked-kernel machinery; the mask is produced by the traversal
instead of by a predicate. The strong oracle is exactly that equivalence: a kernel
over the traversal must be byte-identical to the #114 filter view that keeps the
same edges over the same parent id space.
"""

import math

import pyarrow as pa
import pytest

import ursa as ur
from ursa._execute import _native

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


# A triangle 0->1->2->0 with a pendant tail 2->3->4. hop/shortest_path stay inside a
# region whose induced subgraph is exactly reconstructible by re-ingest.
def _graph():
    return ur.from_arrow(pa.table({"s": [0, 1, 2, 2, 3], "d": [1, 2, 0, 3, 4]}), src="s", dst="d")


def _vals(graph_expr, col):
    return {r["id"]: r[col] for r in graph_expr.collect().to_dicts()}


def _approx(a, b, tol=1e-9):
    assert a.keys() == b.keys()
    for k in a:
        assert math.isclose(a[k], b[k], rel_tol=0, abs_tol=tol), (k, a[k], b[k])


# --- reached set + induced degree -------------------------------------------
def test_hop_kernel_runs_over_induced_subgraph_of_reached_nodes():
    e = _graph()
    # hop out, n=2 from 0: reached {0} + {1} + {2} = {0,1,2}. Induced edges among them:
    # 0->1, 1->2, 2->0 (the triangle); 2->3 is dropped (3 not reached).
    frontier = ur.hop(e, n=2).from_([0])
    deg = _vals(ur.degree(frontier, direction="both"), "degree")
    # nodes 3,4 are not reached -> absent from the region, but the id space is the
    # parent's, so they are present at degree 0.
    assert deg == {0: 2, 1: 2, 2: 2, 3: 0, 4: 0}


def test_reached_node_isolated_by_the_mask_stays_present_at_degree_zero():
    e = _graph()
    # hop out n=3 from 0 reaches {0,1,2,3}. Induced edges: triangle + 2->3. Node 3 has
    # one incident edge (2->3); node 4 is unreached -> present, degree 0.
    frontier = ur.hop(e, n=3).from_([0])
    deg = _vals(ur.degree(frontier, direction="both"), "degree")
    assert deg == {0: 2, 1: 2, 2: 3, 3: 1, 4: 0}


def test_multiple_seeds_union_the_reached_region():
    e = _graph()
    # seeds {0,3}, out n=1: from 0 -> {0,1}; from 3 -> {3,4}; union {0,1,3,4}.
    # induced edges among them: 0->1 only (3->4 is present too). 2 unreached.
    frontier = ur.hop(e, n=1).from_([0, 3])
    deg = _vals(ur.degree(frontier, direction="out"), "degree")
    assert deg == {0: 1, 1: 0, 2: 0, 3: 1, 4: 0}


def test_in_direction_hop_walks_backwards():
    e = _graph()
    # hop in, n=1 from 0: in-neighbours of 0 are {2} (2->0); reached {0,2}.
    # induced edges among {0,2}: 2->0. out-degree: node 2 -> 1, others 0.
    frontier = ur.hop(e, n=1, direction="in").from_([0])
    deg = _vals(ur.degree(frontier, direction="out"), "degree")
    assert deg == {0: 0, 1: 0, 2: 1, 3: 0, 4: 0}


# --- the traversal reduces to the identical #114 mask over the same parent ----
def test_hop_kernel_equals_the_equivalent_filter_subgraph_view():
    # The whole design claim: a kernel over a traversal result is a #114 subgraph view
    # whose mask happens to be produced by the traversal. So it must be *byte-identical*
    # to the filter view that keeps the same edges over the same parent id space.
    e = _graph()
    # hop out n=2 from 0 reaches {0,1,2}; induced edges = both endpoints < 3.
    frontier = ur.hop(e, n=2).from_([0])
    view = e.filter((ur.col("s") < 3) & (ur.col("d") < 3))
    _approx(_vals(ur.pagerank(frontier), "pagerank"), _vals(ur.pagerank(view), "pagerank"))
    assert _vals(ur.degree(frontier, direction="both"), "degree") == _vals(
        ur.degree(view, direction="both"), "degree"
    )
    for mode in ("weak", "strong"):
        got = _vals(ur.connected_components(frontier, mode=mode), "connected_components")
        want = _vals(ur.connected_components(view, mode=mode), "connected_components")
        assert got == want  # same parent id space -> even the labels agree


# --- shortest_path ----------------------------------------------------------
def test_shortest_path_kernel_runs_over_path_node_induced_subgraph():
    # Diamond: 0->1->3 and 0->2->3, plus a chord 3->0. Unweighted path 0->3 is one of
    # the two 2-edge routes; whichever nodes it visits, the induced subgraph includes
    # the 3->0 chord among path nodes.
    e = ur.from_arrow(pa.table({"s": [0, 0, 1, 2, 3], "d": [1, 2, 3, 3, 0]}), src="s", dst="d")
    sp = ur.shortest_path(e, 0, 3)
    deg = _vals(ur.degree(sp, direction="both"), "degree")
    # path nodes are {0, X, 3} for X in {1,2}; the other middle node is absent (deg 0).
    # 0 and 3 each carry 2 incident edges (in-path edge + the 3->0 chord); X carries 2.
    assert deg[0] == 2 and deg[3] == 2
    assert sum(1 for v in deg.values() if v == 0) == 1  # exactly one middle node excluded


def test_weighted_shortest_path_selects_a_different_region():
    # 0->1->3 costs 2; 0->2->3 costs 10. Weighted picks 0->1->3, so node 2 is excluded
    # while the unweighted BFS might have picked either.
    e = ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2, 3], "d": [1, 2, 3, 3, 0], "w": [1.0, 1.0, 1.0, 9.0, 1.0]}),
        src="s",
        dst="d",
    )
    spw = ur.shortest_path(e, 0, 3, weight=ur.col("w"))
    deg = _vals(ur.degree(spw, direction="both"), "degree")
    # region = {0,1,3}; node 2 excluded (present, degree 0).
    assert deg[2] == 0
    assert deg[0] == 2 and deg[1] == 2 and deg[3] == 2


def test_no_path_yields_an_all_excluded_region():
    e = _graph()
    sp = ur.shortest_path(e, 0, 99)  # unknown target -> no path -> empty reached set
    deg = _vals(ur.degree(sp, direction="both"), "degree")
    assert deg == {0: 0, 1: 0, 2: 0, 3: 0, 4: 0}


# --- no rebuild + index sharing ---------------------------------------------
def test_traversal_kernel_reuses_the_parent_csr_no_rebuild():
    e = _graph()
    before = _native()._topology_build_count()
    ur.degree(e).collect()  # build the parent CSR once
    assert _native()._topology_build_count() - before == 1
    # Any number of traversal-result kernels reuse that same CSR.
    ur.pagerank(ur.hop(e, n=2).from_([0])).collect()
    ur.degree(ur.hop(e, n=1, direction="in").from_([2])).collect()
    ur.degree(ur.shortest_path(e, 0, 4)).collect()
    assert _native()._topology_build_count() - before == 1


# --- guards -----------------------------------------------------------------
def test_relational_tail_between_traversal_and_kernel_raises():
    e = _graph()
    frontier = ur.hop(e, n=2).from_([0]).distinct()
    with pytest.raises(NotImplementedError, match="bare hop/shortest_path"):
        ur.pagerank(frontier).collect()


def test_traversal_of_a_traversal_still_raises():
    e = _graph()
    inner = ur.hop(e, n=1).from_([0])
    with pytest.raises(NotImplementedError, match="traversal result"):
        ur.hop(inner, n=1).from_([0]).collect()
