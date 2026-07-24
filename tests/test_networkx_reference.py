"""Cross-check the kernels against NetworkX on a nontrivial graph (#53.2).

All other golden values in the suite are hand-derived on tiny (<=8-node) graphs.
Convergence-sensitive kernels (pagerank, betweenness, closeness, louvain
modularity) are exactly where subtle wrongness hides at scale, so here they are
validated against NetworkX on a seeded ~100-node random graph.

Two documented Ursa/NetworkX divergences are deliberately avoided:
- **parallel-edge sigma counting** — the random graph is simple (no parallel
  edges), so it does not arise;
- **exact-float weighted tie-breaking** — only the unweighted kernels are
  cross-checked here.

Definition alignment (verified against the kernel docstrings):
- closeness is out-edge `reachable / Σ dist`, no Wasserman-Faust scaling — that is
  exactly `nx.closeness_centrality(G.reverse(), wf_improved=False)`;
- betweenness is raw (unnormalized) directed dependency — `normalized=False`;
- triangle/clustering treat edges as undirected — compared on `nx.Graph(edges)`.
"""

import pyarrow as pa
import pytest

import ursa as ur

nx = pytest.importorskip("networkx")

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _graph():
    """A seeded directed random graph, plus the Ursa edge frame built from exactly
    its edge list (so both engines see the identical node set — nodes only exist
    where an edge references them)."""
    src = nx.gnp_random_graph(100, 0.05, seed=42, directed=True)
    edges = list(src.edges())
    gd = nx.DiGraph()
    gd.add_edges_from(edges)
    gu = nx.Graph()
    gu.add_edges_from(edges)
    s = [int(a) for a, _ in edges]
    d = [int(b) for _, b in edges]
    frame = ur.from_arrow(pa.table({"src": s, "dst": d}), src="src", dst="dst")
    return gd, gu, frame


def _rows(frame, col):
    return {r["id"]: r[col] for r in frame.collect().to_dicts()}


def test_pagerank_matches_networkx():
    gd, _, edges = _graph()
    ref = nx.pagerank(gd, alpha=0.85, tol=1e-12, max_iter=1000)
    got = _rows(ur.pagerank(edges, damping=0.85, max_iter=500, tol=1e-12), "pagerank")
    assert set(got) == set(ref)
    for node in ref:
        assert got[node] == pytest.approx(ref[node], abs=1e-6)


def test_betweenness_matches_networkx():
    gd, _, edges = _graph()
    ref = nx.betweenness_centrality(gd, normalized=False, endpoints=False)
    got = _rows(ur.betweenness(edges), "betweenness")
    assert set(got) == set(ref)
    for node in ref:
        assert got[node] == pytest.approx(ref[node], abs=1e-6)


def test_closeness_matches_networkx():
    gd, _, edges = _graph()
    # nx measures incoming distance; reverse to get our outgoing form. wf_improved
    # =False drops the (n-1) scaling, matching our `reachable / Σ dist`.
    ref = nx.closeness_centrality(gd.reverse(), wf_improved=False)
    got = _rows(ur.closeness(edges), "closeness")
    assert set(got) == set(ref)
    for node in ref:
        assert got[node] == pytest.approx(ref[node], abs=1e-9)


def test_triangle_count_matches_networkx():
    _, gu, edges = _graph()
    ref = nx.triangles(gu)
    got = _rows(ur.triangle_count(edges), "triangle_count")
    assert set(got) == set(ref)
    for node in ref:
        assert got[node] == ref[node]


def test_clustering_coefficient_matches_networkx():
    _, gu, edges = _graph()
    ref = nx.clustering(gu)
    got = _rows(ur.clustering_coefficient(edges), "clustering_coefficient")
    assert set(got) == set(ref)
    for node in ref:
        assert got[node] == pytest.approx(ref[node], abs=1e-9)


def test_louvain_modularity_is_competitive_with_networkx():
    # Partitions won't be identical (both are heuristics), so compare the objective
    # they optimize: our partition's modularity should be within a small margin of
    # NetworkX's on the same undirected graph.
    _, gu, edges = _graph()
    got = _rows(ur.louvain(edges, resolution=1.0, seed=1), "louvain")
    ours = _partition_communities(got)
    ref = nx.community.louvain_communities(gu, resolution=1.0, seed=1)
    q_ours = nx.community.modularity(gu, ours, resolution=1.0)
    q_ref = nx.community.modularity(gu, ref, resolution=1.0)
    # Ours must be a genuine modularity maximum, not merely non-negative: no worse
    # than NetworkX by more than a small slack.
    assert q_ours >= q_ref - 0.05, f"ours={q_ours:.4f} nx={q_ref:.4f}"


def _partition_communities(labels: dict) -> list[set]:
    groups: dict = {}
    for node, label in labels.items():
        groups.setdefault(label, set()).add(node)
    return list(groups.values())
