"""Cross-check a computed result against the reference, within tolerance.

Every timed cell also produces a result; this module decides whether that result
*agrees* with the NetworkX reference for the same (algorithm, dataset). A fast
wrong answer must score zero — an ``incorrect`` row is a correctness bug, as
loud a signal as a slow one is a perf gap.

The comparison mode comes from the algorithm's ``compare`` tag:

* ``value_map`` — float scores agree within an absolute tolerance;
* ``int_map`` — integer counts agree exactly;
* ``partition`` — labels induce the *same grouping* (label identities are
  arbitrary across libraries, so we compare the partition, not the numbers).
"""

from __future__ import annotations

from .algorithms import Algorithm


def compare(
    reference: dict[int, float] | None,
    candidate: dict[int, float],
    algo: Algorithm,
    edges: list[tuple[int, int]] | None = None,
) -> tuple[bool | None, str]:
    """Return ``(correct, detail)``. ``correct is None`` when not checkable.

    ``edges`` is only needed by the ``modularity`` mode (community detection),
    which scores the partition against the graph rather than comparing labels.
    """
    if reference is None:
        return None, "no reference (dataset too large for the oracle, or oracle skipped)"

    # top-K compares the *selected set*, which legitimately differs from the full
    # node set — check it before the whole-node-set guard below.
    if algo.compare == "topk":
        return _topk(reference, candidate)

    if set(reference) != set(candidate):
        missing = len(set(reference) - set(candidate))
        extra = len(set(candidate) - set(reference))
        return False, f"node-set mismatch (missing {missing}, extra {extra})"

    if algo.compare == "value_map":
        return _value_map(reference, candidate, algo.tol)
    if algo.compare == "int_map":
        return _int_map(reference, candidate)
    if algo.compare == "partition":
        return _partition(reference, candidate)
    if algo.compare == "modularity":
        return _modularity(reference, candidate, algo, edges)
    return None, f"unknown compare mode {algo.compare!r}"


def _value_map(ref: dict, cand: dict, tol: float) -> tuple[bool, str]:
    max_diff = 0.0
    worst = None
    for node, rv in ref.items():
        diff = abs(float(cand[node]) - float(rv))
        if diff > max_diff:
            max_diff, worst = diff, node
    ok = max_diff <= tol
    detail = f"max |Δ|={max_diff:.3e} (tol {tol:.0e})"
    if not ok:
        detail += f" at node {worst}"
    return ok, detail


def _int_map(ref: dict, cand: dict) -> tuple[bool, str]:
    mismatches = [n for n, rv in ref.items() if int(cand[n]) != int(rv)]
    if not mismatches:
        return True, "exact"
    return False, f"{len(mismatches)} node(s) differ (e.g. {mismatches[0]})"


def _partition(ref: dict, cand: dict) -> tuple[bool, str]:
    ref_groups = _groups(ref)
    cand_groups = _groups(cand)
    if ref_groups == cand_groups:
        return True, f"{len(ref_groups)} components match"
    return False, f"partition differs ({len(cand_groups)} vs {len(ref_groups)} components)"


# A pipeline's top-K set may churn slightly at the boundary because PageRank
# values tie or differ by convergence noise across libraries; require most of the
# set to agree rather than an exact match.
_TOPK_MIN_OVERLAP = 0.8


def _topk(reference: dict[int, float], candidate: dict[int, float]) -> tuple[bool, str]:
    """Compare two pipeline results by the overlap of their selected node sets."""
    ref_ids, cand_ids = set(reference), set(candidate)
    if not ref_ids:
        return None, "empty reference selection"
    overlap = len(ref_ids & cand_ids) / len(ref_ids)
    ok = overlap >= _TOPK_MIN_OVERLAP
    return ok, f"top-{len(ref_ids)} overlap {overlap:.0%} (need {_TOPK_MIN_OVERLAP:.0%})"


def _groups(labels: dict[int, float]) -> set[frozenset[int]]:
    by_label: dict[float, set[int]] = {}
    for node, label in labels.items():
        by_label.setdefault(label, set()).add(node)
    return {frozenset(members) for members in by_label.values()}


# Slack allowed on the modularity objective: a heuristic partition need only be
# *competitive* with the oracle's, not identical. A real bug (a degenerate or
# scrambled partition) loses far more than this.
_MODULARITY_SLACK = 0.05


def _modularity(
    reference: dict[int, float],
    candidate: dict[int, float],
    algo: Algorithm,
    edges: list[tuple[int, int]] | None,
) -> tuple[bool | None, str]:
    """Score community detection by the objective it optimises, not by labels.

    Two heuristics never return identical partitions, so we compare the modularity
    each achieves on the same undirected graph: the candidate passes if its Q is no
    worse than the oracle's by more than a small slack.
    """
    if edges is None:
        return None, "no edges available for modularity scoring"
    import networkx as nx

    graph = nx.Graph()
    graph.add_nodes_from(reference)
    graph.add_edges_from(edges)
    resolution = float(algo.params.get("resolution", 1.0))

    def q(labels: dict[int, float]) -> float:
        communities = [set(g) for g in _groups(labels)]
        return nx.community.modularity(graph, communities, resolution=resolution)

    q_cand = q(candidate)
    q_ref = q(reference)
    ok = q_cand >= q_ref - _MODULARITY_SLACK
    detail = f"Q={q_cand:.4f} vs oracle {q_ref:.4f} (slack {_MODULARITY_SLACK})"
    return ok, detail
