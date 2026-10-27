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


def test_strong_components_match_networkx():
    # SCC labels are arbitrary ids, so compare the induced partition (set of node
    # sets) against NetworkX's strongly_connected_components on the same digraph.
    gd, _, edges = _graph()
    got = _rows(ur.connected_components(edges, mode="strong"), "connected_components")
    ours = {frozenset(group) for group in _partition_communities(got)}
    ref = {frozenset(int(n) for n in comp) for comp in nx.strongly_connected_components(gd)}
    assert ours == ref


def _partition_communities(labels: dict) -> list[set]:
    groups: dict = {}
    for node, label in labels.items():
        groups.setdefault(label, set()).add(node)
    return list(groups.values())


# --- direction="both": the undirected view (#124) ---------------------------
#
# An EdgeFrame is directed; `direction="both"` reads it as the simple undirected
# graph — one edge per distinct unordered pair, self-loops dropped — which is
# NetworkX's `G.to_undirected()`. Karate stores each undirected edge once (the
# exact trap #124 reports); the random digraph above has reciprocal pairs, which
# must collapse to one undirected edge.


def _karate():
    edges = ur.datasets.load_karate()
    rows = edges.collect().to_dicts()
    gd = nx.DiGraph()
    gd.add_edges_from((r["src"], r["dst"]) for r in rows)
    return gd, gd.to_undirected(), edges


def _assert_close(got: dict, ref: dict, abs_tol: float) -> None:
    assert set(got) == set(ref)
    for node in ref:
        assert got[node] == pytest.approx(ref[node], abs=abs_tol), node


@pytest.mark.parametrize("graph", ["karate", "random"])
def test_both_whole_graph_stats_match_networkx_undirected(graph):
    _, gu, edges = _karate() if graph == "karate" else _graph()
    assert ur.density(edges, direction="both") == pytest.approx(nx.density(gu), abs=1e-12)
    summary = ur.describe(edges, full=True, direction="both").collect().to_dicts()[0]
    assert summary["n_nodes"] == gu.number_of_nodes()
    assert summary["n_edges"] == gu.number_of_edges()
    assert summary["density"] == pytest.approx(nx.density(gu), abs=1e-12)
    mean_degree = 2 * gu.number_of_edges() / gu.number_of_nodes()
    assert summary["avg_degree"] == pytest.approx(mean_degree, abs=1e-12)
    assert summary["n_components"] == nx.number_connected_components(gu)
    if nx.is_connected(gu):
        ref_apl = nx.average_shortest_path_length(gu)
        assert ur.avg_path_length(edges, direction="both") == pytest.approx(ref_apl, abs=1e-12)
        assert ur.diameter(edges, approximate=False, direction="both") == nx.diameter(gu)


def test_both_stats_on_karate_are_the_textbook_values():
    # The numbers #124's user expected (and NetworkX prints) for the karate club.
    edges = ur.datasets.load_karate()
    assert ur.density(edges, direction="both") == pytest.approx(78 / (34 * 33 / 2))
    assert ur.diameter(edges, approximate=False, direction="both") == 5
    assert ur.diameter(edges, direction="both") == 5  # n < 128: every source runs
    assert ur.avg_path_length(edges, direction="both") == pytest.approx(2.408199643)


@pytest.mark.parametrize("graph", ["karate", "random"])
def test_both_centralities_match_networkx_undirected(graph):
    _, gu, edges = _karate() if graph == "karate" else _graph()
    pr = nx.pagerank(gu, alpha=0.85, tol=1e-12, max_iter=1000)
    got = _rows(ur.pagerank(edges, max_iter=500, tol=1e-12, direction="both"), "pagerank")
    _assert_close(got, pr, 1e-9)

    bc = nx.betweenness_centrality(gu, normalized=False)
    _assert_close(_rows(ur.betweenness(edges, direction="both"), "betweenness"), bc, 1e-9)

    cl = nx.closeness_centrality(gu, wf_improved=False)
    _assert_close(_rows(ur.closeness(edges, direction="both"), "closeness"), cl, 1e-12)


def test_closeness_in_is_networkx_directed_closeness():
    # NetworkX's directed closeness measures distance *to* the node: direction="in".
    gd, _, edges = _graph()
    ref = nx.closeness_centrality(gd, wf_improved=False)
    _assert_close(_rows(ur.closeness(edges, direction="in"), "closeness"), ref, 1e-12)


def test_both_weighted_centralities_match_networkx_undirected():
    # Integer weights keep path sums exact, so tie handling cannot diverge. The
    # weight depends only on the unordered pair, so a reciprocal pair carries one
    # weight either way and NetworkX's to_undirected() has no choice to make.
    gd, _, _ = _graph()
    wt = {(u, v): float((min(u, v) * 7 + max(u, v) * 3) % 5 + 1) for u, v in gd.edges()}
    nx.set_edge_attributes(gd, wt, "weight")
    gu = gd.to_undirected()
    pairs = list(gd.edges())
    edges = ur.from_arrow(
        pa.table(
            {
                "src": [u for u, _ in pairs],
                "dst": [v for _, v in pairs],
                "w": [wt[e] for e in pairs],
            }
        ),
        src="src",
        dst="dst",
    )
    w = ur.col("w")
    cl = nx.closeness_centrality(gu, distance="weight", wf_improved=False)
    _assert_close(_rows(ur.closeness(edges, weight=w, direction="both"), "closeness"), cl, 1e-12)
    bc = nx.betweenness_centrality(gu, normalized=False, weight="weight")
    got_bc = _rows(ur.betweenness(edges, weight=w, direction="both"), "betweenness")
    _assert_close(got_bc, bc, 1e-9)
    pr = nx.pagerank(gu, alpha=0.85, tol=1e-12, max_iter=1000, weight="weight")
    got_pr = _rows(
        ur.pagerank(edges, weight=w, max_iter=500, tol=1e-12, direction="both"), "pagerank"
    )
    _assert_close(got_pr, pr, 1e-9)


def test_default_direction_is_unchanged_on_karate():
    # Defaults stay directed ("out"): identical to passing direction="out", and to
    # the directed NetworkX values over the rows as stored.
    gd, _, edges = _karate()
    n = gd.number_of_nodes()
    assert ur.density(edges) == ur.density(edges, direction="out") == 78 / (n * (n - 1))
    dists = [d for _, row in nx.all_pairs_shortest_path_length(gd) for d in row.values() if d > 0]
    assert ur.avg_path_length(edges) == pytest.approx(sum(dists) / len(dists), abs=1e-12)
    assert ur.diameter(edges, approximate=False) == max(dists)
    assert ur.describe(edges).collect().to_dicts()[0]["n_edges"] == 78
    refs = {
        "pagerank": nx.pagerank(gd, tol=1e-12, max_iter=1000),
        "betweenness": nx.betweenness_centrality(gd, normalized=False),
        "closeness": nx.closeness_centrality(gd.reverse(), wf_improved=False),
    }
    for name, ref in refs.items():
        verb = getattr(ur, name)
        kwargs = {"max_iter": 500, "tol": 1e-12} if name == "pagerank" else {}
        default = _rows(verb(edges, **kwargs), name)
        assert default == _rows(verb(edges, direction="out", **kwargs), name)
        _assert_close(default, ref, 1e-9)


def test_direction_is_validated_at_the_call_site():
    edges = ur.datasets.load_karate()
    for fn in (ur.density, ur.avg_path_length, ur.diameter, ur.describe, ur.closeness):
        with pytest.raises(ValueError, match="direction"):
            fn(edges, direction="sideways")
    for fn in (ur.pagerank, ur.betweenness):
        with pytest.raises(ValueError, match="'in' is not supported"):
            fn(edges, direction="in")


def test_from_networkx_symmetrizes_an_undirected_graph():
    # #124 suggestion 2: an undirected nx.Graph must not silently become a
    # one-direction digraph. Each edge arrives in both directions, so the default
    # direction="out" already sees the undirected graph.
    g = nx.karate_club_graph()
    edges = ur.from_networkx(g)
    assert edges.collect().to_arrow().num_rows == 2 * g.number_of_edges()
    pr = nx.pagerank(g, tol=1e-12, max_iter=1000, weight=None)
    _assert_close(_rows(ur.pagerank(edges, max_iter=500, tol=1e-12), "pagerank"), pr, 1e-9)
    cl = nx.closeness_centrality(g, wf_improved=False)
    _assert_close(_rows(ur.closeness(edges), "closeness"), cl, 1e-12)
    assert ur.density(edges) == pytest.approx(nx.density(g))
    assert ur.diameter(edges, approximate=False) == nx.diameter(g)
    # direction="both" on the symmetrized frame is the same undirected graph.
    bc = nx.betweenness_centrality(g, normalized=False)
    _assert_close(_rows(ur.betweenness(edges, direction="both"), "betweenness"), bc, 1e-9)

    # Opt out: one row per undirected edge, as before.
    once = ur.from_networkx(g, symmetrize=False)
    assert once.collect().to_arrow().num_rows == g.number_of_edges()
    assert ur.density(once, direction="both") == pytest.approx(nx.density(g))

    # A DiGraph keeps its rows as given; a self-loop is never doubled.
    assert ur.from_networkx(nx.DiGraph([(0, 1), (1, 2)])).collect().to_arrow().num_rows == 2
    assert ur.from_networkx(nx.Graph([(0, 0), (0, 1)])).collect().to_arrow().num_rows == 3


def _symmetric_rows(edges, weights):
    """The simple undirected view as directed rows `u -> v` and `v -> u`, sorted by
    (source, target), each pair at the minimum weight of the rows joining it."""
    best = {}
    for (u, v), w in zip(edges, weights, strict=True):
        if u != v:
            key = (min(u, v), max(u, v))
            best[key] = min(best.get(key, w), w)
    rows = sorted(
        [(u, v, w) for (u, v), w in best.items()] + [(v, u, w) for (u, v), w in best.items()]
    )
    return rows


def test_both_is_bit_identical_to_the_out_kernels_over_the_symmetric_rows():
    # direction="both" runs the directed kernels over the symmetric view. The oracle
    # builds that view by hand as an edge list and runs direction="out" on it. A
    # leading self-loop per node pins the dense ids to 0..n in both frames; the view
    # drops self-loops, and the oracle masks them out with a filter.
    import random

    rng = random.Random(7)
    n = 60
    pairs = [(rng.randrange(n), rng.randrange(n)) for _ in range(400)]
    pairs += [(v, u) for u, v in pairs[:50]] + pairs[50:80]  # reciprocal + parallel
    weights = [float(rng.randrange(1, 6)) for _ in pairs]
    loops = list(range(n))
    graph = ur.from_arrow(
        pa.table(
            {
                "src": loops + [u for u, _ in pairs],
                "dst": loops + [v for _, v in pairs],
                "w": [1.0] * n + weights,
            }
        ),
        src="src",
        dst="dst",
    )
    sym = _symmetric_rows(pairs, weights)
    oracle = ur.from_arrow(
        pa.table(
            {
                "src": loops + [u for u, _, _ in sym],
                "dst": loops + [v for _, v, _ in sym],
                "w": [1.0] * n + [w for _, _, w in sym],
            }
        ),
        src="src",
        dst="dst",
    ).filter(ur.col("src") != ur.col("dst"))
    w = ur.col("w")

    def both(e):
        return (
            e.nodes()
            .with_columns(
                pr=ur.pagerank(e, direction="both"),
                prw=ur.pagerank(e, weight=w, direction="both"),
                bc=ur.betweenness(e, direction="both"),
                bcw=ur.betweenness(e, weight=w, direction="both"),
            )
            .collect()
            .to_dicts()
        )

    first = both(graph)
    assert both(graph) == first  # the second query reuses the cached view
    assert both(graph.filter(ur.col("w") > 0.0)) == first  # the per-query masked build
    want = (
        oracle.nodes()
        .with_columns(
            pr=ur.pagerank(oracle),
            prw=ur.pagerank(oracle, weight=w),
            bc=ur.betweenness(oracle),
            bcw=ur.betweenness(oracle, weight=w),
        )
        .collect()
        .to_dicts()
    )
    for row in want:
        row["bc"] *= 0.5
        row["bcw"] *= 0.5
    assert first == want
