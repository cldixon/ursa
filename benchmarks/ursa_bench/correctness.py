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
) -> tuple[bool | None, str]:
    """Return ``(correct, detail)``. ``correct is None`` when not checkable."""
    if reference is None:
        return None, "no reference (dataset too large for the oracle, or oracle skipped)"

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
        # Heuristic community detection: identical partitions aren't expected, so
        # a value/label comparison is meaningless. Left for a modularity-scoring
        # check (needs the graph, not just the labels) — reported as not-checkable.
        return None, "modularity comparison not wired (needs graph-aware scoring)"
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


def _groups(labels: dict[int, float]) -> set[frozenset[int]]:
    by_label: dict[float, set[int]] = {}
    for node, label in labels.items():
        by_label.setdefault(label, set()).add(node)
    return {frozenset(members) for members in by_label.values()}
