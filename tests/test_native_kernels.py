"""End-to-end tests of the native kernels (Python → PyO3 → ursa-core).

Skipped automatically if the native extension has not been built
(``maturin develop``). When built, these prove the real Rust kernels run and
return correct answers through the Python boundary via the public ``collect()``
path.
"""

import re

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges(src, dst):
    return ur.from_arrow(pa.table({"s": src, "d": dst}), src="s", dst="d")


def _rows(frame, col):
    return {r["id"]: r[col] for r in frame.collect().to_dicts()}


def test_core_version_loads():
    # Version is single-sourced from the Cargo workspace, so assert its shape
    # (a semver string) rather than a literal that breaks on every bump.
    version = ur.__core_version__
    assert isinstance(version, str)
    assert re.fullmatch(r"\d+\.\d+\.\d+", version)


def test_version_matches_core():
    # __version__ (installed distribution metadata) and __core_version__ (from the
    # native crate) are both single-sourced from the Cargo workspace version, so an
    # installed build must agree on both. This makes a future drift impossible.
    assert ur.__version__ == ur.__core_version__


def test_degree():
    # 0->1, 0->2, 1->2, 2->0
    edges = _edges([0, 0, 1, 2], [1, 2, 2, 0])
    assert _rows(ur.degree(edges, direction="out"), "degree") == {0: 2, 1: 1, 2: 1}
    assert _rows(ur.degree(edges, direction="in"), "degree") == {0: 1, 1: 1, 2: 2}


def test_pagerank_ranks_hub_highest():
    # everyone points at node 2
    edges = _edges([0, 1, 3, 2], [2, 2, 2, 0])
    scores = _rows(ur.pagerank(edges), "pagerank")
    assert abs(sum(scores.values()) - 1.0) < 1e-6
    assert scores[2] == max(scores.values())


def test_connected_components():
    # {0,1,2} and {3,4}
    edges = _edges([0, 1, 3], [1, 2, 4])
    labels = _rows(ur.connected_components(edges), "connected_components")
    assert labels[0] == labels[1] == labels[2]
    assert labels[3] == labels[4]
    assert labels[0] != labels[3]
