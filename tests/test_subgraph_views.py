"""Subgraph views (#114): a filtered EdgeFrame is a *view* over the parent CSR.

`edges.filter(pred)` no longer rebuilds the topology — the parent CSR runs
restricted by a per-edge-row boolean mask (the predicate evaluated over the parent
edge rows). The id space and edge order are preserved, so:

  * no CSR rebuild happens (observable via `_topology_build_count`);
  * every node-valued kernel computes over the masked edge set;
  * a node with zero unmasked incident edges stays *present* with degree 0;
  * repeated `.filter()` **intersects** (logical AND);
  * a filter that keeps every row is byte-identical to the full graph.

The strong-correctness oracle re-ingests exactly the kept edges via `from_arrow`
and compares — valid only when the filter leaves no node isolated (re-ingesting
edges can't carry an isolated node), so those graphs are chosen accordingly. Node
presence under isolation is covered separately with degree.
"""

import math

import pyarrow as pa
import pytest

import ursa as ur
from ursa._execute import _native

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


# A 5-cycle 0->1->2->3->4->0 (labeled "cyc") plus two "extra" chords. Filtering to
# the cycle edges keeps every node non-isolated, so a re-ingest of just those edges
# is an exact oracle for the view.
def _labeled():
    return ur.from_arrow(
        pa.table(
            {
                "s": [0, 1, 2, 3, 4, 0, 2],
                "d": [1, 2, 3, 4, 0, 2, 4],
                "kind": ["cyc", "cyc", "cyc", "cyc", "cyc", "extra", "extra"],
                "w": [1.0, 1.0, 1.0, 1.0, 1.0, 5.0, 7.0],
            }
        ),
        src="s",
        dst="d",
    )


def _cycle_only():
    return ur.from_arrow(pa.table({"s": [0, 1, 2, 3, 4], "d": [1, 2, 3, 4, 0]}), src="s", dst="d")


def _vals(graph_expr, col):
    return {r["id"]: r[col] for r in graph_expr.collect().to_dicts()}


def _approx(a, b, tol=1e-9):
    assert a.keys() == b.keys()
    for k in a:
        assert math.isclose(a[k], b[k], rel_tol=0, abs_tol=tol), (k, a[k], b[k])


# --- no rebuild -------------------------------------------------------------
def test_filter_reuses_the_parent_csr_no_rebuild():
    edges = _labeled()
    before = _native()._topology_build_count()
    # Build the parent CSR once (full-graph op).
    ur.degree(edges).collect()
    built_parent = _native()._topology_build_count() - before
    assert built_parent == 1
    # A graph op over any number of subgraph views reuses that same CSR — no rebuild.
    ur.degree(edges.filter(ur.col("kind") == "cyc")).collect()
    ur.pagerank(edges.filter(ur.col("s") < 3)).collect()
    ur.connected_components(edges.filter(ur.col("w") > 1.0)).collect()
    assert _native()._topology_build_count() - before == 1


def test_subgraph_view_shares_the_parent_index_cell():
    edges = _labeled()
    view = edges.filter(ur.col("kind") == "cyc")
    assert view._index_build_cell is edges._index_build_cell


# --- correctness vs a re-ingested oracle (no node isolated) -----------------
def test_pagerank_over_view_matches_reingested_subgraph():
    view = _labeled().filter(ur.col("kind") == "cyc")
    _approx(_vals(ur.pagerank(view), "pagerank"), _vals(ur.pagerank(_cycle_only()), "pagerank"))


def test_degree_over_view_matches_reingested_subgraph():
    view = _labeled().filter(ur.col("kind") == "cyc")
    for direction in ("in", "out", "both"):
        got = _vals(ur.degree(view, direction=direction), "degree")
        want = _vals(ur.degree(_cycle_only(), direction=direction), "degree")
        assert got == want


def test_closeness_over_view_matches_reingested_subgraph():
    view = _labeled().filter(ur.col("kind") == "cyc")
    _approx(_vals(ur.closeness(view), "closeness"), _vals(ur.closeness(_cycle_only()), "closeness"))


def test_components_over_view_match_reingested_subgraph():
    view = _labeled().filter(ur.col("kind") == "cyc")
    for mode in ("weak", "strong"):
        got = _vals(ur.connected_components(view, mode=mode), "connected_components")
        want = _vals(ur.connected_components(_cycle_only(), mode=mode), "connected_components")
        # Component *labels* are arbitrary; compare the induced partition.
        assert _partition(got) == _partition(want)


def test_louvain_over_view_matches_reingested_subgraph():
    view = _labeled().filter(ur.col("kind") == "cyc")
    got = _vals(ur.louvain(view, seed=7), "louvain")
    want = _vals(ur.louvain(_cycle_only(), seed=7), "louvain")
    assert _partition(got) == _partition(want)


def test_label_propagation_over_view_matches_reingested_subgraph():
    view = _labeled().filter(ur.col("kind") == "cyc")
    got = _vals(ur.label_propagation(view, seed=7), "label_propagation")
    want = _vals(ur.label_propagation(_cycle_only(), seed=7), "label_propagation")
    assert _partition(got) == _partition(want)


def _partition(labels):
    groups: dict = {}
    for node, lbl in labels.items():
        groups.setdefault(lbl, set()).add(node)
    return frozenset(frozenset(g) for g in groups.values())


# --- byte-identity: a keep-all filter == the full graph ---------------------
def test_keep_all_filter_is_identical_to_full_graph():
    edges = _labeled()
    keep_all = edges.filter(ur.col("s") >= 0)  # every row satisfies s >= 0
    _approx(_vals(ur.pagerank(keep_all), "pagerank"), _vals(ur.pagerank(edges), "pagerank"))
    assert _vals(ur.degree(keep_all), "degree") == _vals(ur.degree(edges), "degree")


# --- node presence under isolation ------------------------------------------
def test_node_isolated_by_the_view_stays_present_with_degree_zero():
    # Star: 0 -> {1,2,3}. Filtering to dst == 1 leaves nodes 2 and 3 with no incident
    # edge, but they must remain present at degree 0 (the id space is the parent's).
    edges = ur.from_arrow(pa.table({"s": [0, 0, 0], "d": [1, 2, 3]}), src="s", dst="d")
    view = edges.filter(ur.col("d") == 1)
    deg = _vals(ur.degree(view, direction="both"), "degree")
    assert deg == {0: 1, 1: 1, 2: 0, 3: 0}


def test_endpoint_filter_on_src_selects_incident_edges():
    edges = ur.from_arrow(pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0]}), src="s", dst="d")
    view = edges.filter(ur.col("s") == 0)  # keep only edges leaving node 0
    out = _vals(ur.degree(view, direction="out"), "degree")
    assert out == {0: 2, 1: 0, 2: 0}


# --- repeated filter intersects (logical AND) -------------------------------
def test_repeated_filter_intersects():
    edges = ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2, 3], "d": [1, 2, 0, 0, 0], "w": [1, 9, 1, 1, 9]}),
        src="s",
        dst="d",
    )
    # s < 3 keeps {0->1,0->2,1->0,2->0}; AND w > 1 keeps only {0->2}.
    view = edges.filter(ur.col("s") < 3).filter(ur.col("w") > 1)
    indeg = _vals(ur.degree(view, direction="in"), "degree")
    assert indeg == {0: 0, 1: 0, 2: 1, 3: 0}
    # Order of the two filters does not change the intersection.
    view2 = edges.filter(ur.col("w") > 1).filter(ur.col("s") < 3)
    assert _vals(ur.degree(view2, direction="in"), "degree") == indeg


# --- weighted kernels over the view -----------------------------------------
def test_weighted_pagerank_over_view_matches_reingested_subgraph():
    # Keep the cycle edges (all w == 1) so the re-ingest carries the same weights.
    view = _labeled().filter(ur.col("kind") == "cyc")
    got = _vals(ur.pagerank(view, weight=ur.col("w")), "pagerank")
    cyc = ur.from_arrow(
        pa.table({"s": [0, 1, 2, 3, 4], "d": [1, 2, 3, 4, 0], "w": [1.0] * 5}),
        src="s",
        dst="d",
    )
    want = _vals(ur.pagerank(cyc, weight=ur.col("w")), "pagerank")
    _approx(got, want)


# --- triangle / clustering (the undirected-rebuild path) --------------------
def test_triangle_and_clustering_over_view():
    # A triangle 0-1-2 plus a pendant 2->3. Filtering out the pendant leaves exactly
    # the triangle; the undirected masked rebuild must see one triangle per node.
    edges = ur.from_arrow(
        pa.table({"s": [0, 1, 2, 2], "d": [1, 2, 0, 3], "keep": [1, 1, 1, 0]}),
        src="s",
        dst="d",
    )
    view = edges.filter(ur.col("keep") == 1)
    tri = _vals(ur.triangle_count(view), "triangle_count")
    assert tri == {0: 1, 1: 1, 2: 1, 3: 0}
    clus = _vals(ur.clustering_coefficient(view), "clustering_coefficient")
    # Each triangle node has both neighbors connected -> coefficient 1.0; the isolated
    # (in the view) node 3 has no pairs -> 0.0.
    assert math.isclose(clus[0], 1.0) and math.isclose(clus[3], 0.0)
