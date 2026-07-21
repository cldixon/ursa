"""End-to-end tests of the native demo kernels (Python → PyO3 → ursa-core).

Skipped automatically if the native extension has not been built
(``maturin develop``). When built, these prove the real Rust kernels run and
return correct answers through the Python boundary.
"""

import re

import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def test_core_version_loads():
    # Version is single-sourced from the Cargo workspace, so assert its shape
    # (a semver string) rather than a literal that breaks on every bump.
    version = ur.__core_version__
    assert isinstance(version, str)
    assert re.fullmatch(r"\d+\.\d+\.\d+", version)


def test_demo_degree():
    # 0->1, 0->2, 1->2, 2->0
    src, dst = [0, 0, 1, 2], [1, 2, 2, 0]
    out = dict(ur.demo.degree(src, dst, direction="out"))
    assert out == {0: 2, 1: 1, 2: 1}
    inc = dict(ur.demo.degree(src, dst, direction="in"))
    assert inc == {0: 1, 1: 1, 2: 2}


def test_demo_pagerank_ranks_hub_highest():
    # everyone points at node 2
    src, dst = [0, 1, 3, 2], [2, 2, 2, 0]
    scores = dict(ur.demo.pagerank(src, dst))
    assert abs(sum(scores.values()) - 1.0) < 1e-6
    assert scores[2] == max(scores.values())


def test_demo_connected_components():
    # {0,1,2} and {3,4}
    src, dst = [0, 1, 3], [1, 2, 4]
    labels = dict(ur.demo.connected_components(src, dst))
    assert labels[0] == labels[1] == labels[2]
    assert labels[3] == labels[4]
    assert labels[0] != labels[3]
