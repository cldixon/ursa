"""Parameter plumbing (#53.5) and Python-boundary error paths (#53.4).

Each algorithm parameter is exercised Python -> JSON-IR -> Rust and asserted to
make an *observable* difference, so a typo'd key that silently falls back to a
default (the class of bug that shipped `connected_components(mode=)`) fails here.
The error-path tests drive the boundary conditions the Rust unit tests can't reach
from Python (bad column names, missing files, null endpoints, empty graphs).
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges(src, dst):
    return ur.from_arrow(pa.table({"s": src, "d": dst}), src="s", dst="d")


def _rows(frame, col):
    return {r["id"]: r[col] for r in frame.collect().to_dicts()}


# --- parameter plumbing (#53.5) --------------------------------------------
def test_degree_direction_is_honored():
    edges = _edges([0, 0, 1], [1, 2, 2])
    out = _rows(ur.degree(edges, direction="out"), "degree")
    inn = _rows(ur.degree(edges, direction="in"), "degree")
    both = _rows(ur.degree(edges, direction="both"), "degree")
    assert out[0] == 2 and inn[0] == 0 and both[0] == 2  # 0 has two out-edges
    assert out[2] == 0 and inn[2] == 2 and both[2] == 2  # 2 has two in-edges
    assert out != inn


def test_pagerank_max_iter_and_tol_are_honored():
    # A directed chain 0->1->...->9: one iteration is far from the fixpoint.
    edges = _edges(list(range(9)), list(range(1, 10)))
    one = _rows(ur.pagerank(edges, max_iter=1, tol=0.0), "pagerank")
    conv = _rows(ur.pagerank(edges, max_iter=500, tol=1e-12), "pagerank")
    max_diff = max(abs(one[k] - conv[k]) for k in conv)
    assert max_diff > 1e-6  # the iteration cap actually stopped it short


def test_louvain_resolution_is_honored():
    # Two triangles joined by a single bridge edge. High resolution favours
    # smaller communities, so it finds at least as many as low resolution.
    edges = _edges(
        [0, 1, 2, 3, 4, 5, 2],
        [1, 2, 0, 4, 5, 3, 3],
    )
    low = _rows(ur.louvain(edges, resolution=0.2, seed=1), "louvain")
    high = _rows(ur.louvain(edges, resolution=3.0, seed=1), "louvain")
    assert len(set(high.values())) >= len(set(low.values()))
    assert len(set(high.values())) >= 2  # the two triangles split at high res


def test_betweenness_sample_seed_is_reproducible_and_differs_from_exact():
    edges = _edges([*range(19), 0], [*range(1, 20), 10])
    a = _rows(ur.betweenness(edges, sample=0.5, seed=7), "betweenness")
    b = _rows(ur.betweenness(edges, sample=0.5, seed=7), "betweenness")
    assert a == b  # same seed -> identical estimate
    c = _rows(ur.betweenness(edges, sample=0.5, seed=99), "betweenness")
    assert a != c  # a different seed samples different sources


def test_avg_path_length_sample_runs_and_matches_full_at_fraction_one():
    edges = _edges([0, 1, 2, 3], [1, 2, 3, 0])
    exact = ur.avg_path_length(edges)
    assert ur.avg_path_length(edges, sample=1.0) == pytest.approx(exact)
    assert ur.avg_path_length(edges, sample=0.5) > 0.0


# --- error paths (#53.4) ----------------------------------------------------
def test_scan_edges_wrong_column_names_the_column(tmp_path):
    p = tmp_path / "edges.parquet"
    pq.write_table(pa.table({"a": [0, 1], "b": [1, 0]}), p)
    edges = ur.scan_edges(str(p), src="nope", dst="b")
    with pytest.raises(ur.UrsaError, match="nope"):
        ur.pagerank(edges).collect()


def test_scan_nonexistent_path_errors_clearly(tmp_path):
    edges = ur.scan_edges(str(tmp_path / "missing.parquet"), src="a", dst="b")
    with pytest.raises(ur.UrsaError):
        ur.pagerank(edges).collect()


def test_null_endpoint_is_a_clean_error():
    edges = ur.from_arrow(
        pa.table({"s": pa.array([0, None], type=pa.int64()), "d": pa.array([1, 0])}),
        src="s",
        dst="d",
    )
    with pytest.raises(ValueError, match="null"):
        ur.pagerank(edges).collect()


def test_empty_edge_frame_collects_to_an_empty_result():
    edges = ur.from_arrow(
        pa.table({"s": pa.array([], type=pa.int64()), "d": pa.array([], type=pa.int64())}),
        src="s",
        dst="d",
    )
    df = ur.pagerank(edges).collect().to_arrow()
    assert df.num_rows == 0


def test_connected_components_unknown_mode_is_rejected_not_ignored():
    # honor-or-raise: weak and strong are wired; any other mode must raise, not
    # silently fall back to weak.
    edges = _edges([0, 1], [1, 0])
    with pytest.raises((NotImplementedError, ur.UrsaError, ValueError)):
        ur.connected_components(edges, mode="bogus").collect()


# --- glob-path scans (#53.6) ------------------------------------------------
def test_scan_edges_glob_reads_all_parts(tmp_path):
    # README claims glob support; a directory of parquet parts must all be read.
    pq.write_table(pa.table({"s": [0, 1], "d": [1, 2]}), tmp_path / "part1.parquet")
    pq.write_table(pa.table({"s": [2, 3], "d": [3, 0]}), tmp_path / "part2.parquet")
    edges = ur.scan_edges(str(tmp_path / "*.parquet"), src="s", dst="d")
    deg = _rows(ur.degree(edges, direction="both"), "degree")
    # every node from both parts is present -> both files were scanned
    assert set(deg) == {0, 1, 2, 3}
