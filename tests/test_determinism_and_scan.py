"""Determinism (#40) and single-scan weighted alignment (#37).

- Stochastic/parallel kernels give the same result on repeated same-seed runs
  (Louvain tie-breaks, PageRank f64 reductions, sampled betweenness).
- Sampled betweenness takes a reproducible seed=.
- A weighted algorithm over a multi-part glob matches the same data in one file
  (endpoints and weights are read in a single scan, so edge order can't drift).
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _two_cliques():
    pairs = [
        (0, 1), (0, 2), (0, 3), (1, 2), (1, 3), (2, 3),
        (4, 5), (4, 6), (4, 7), (5, 6), (5, 7), (6, 7),
        (3, 4),
    ]  # fmt: skip
    return ur.from_arrow(
        pa.table({"s": [a for a, _ in pairs], "d": [b for _, b in pairs]}), src="s", dst="d"
    )


# --- #40.2: PageRank is bit-reproducible -----------------------------------
def test_pagerank_is_bit_reproducible():
    edges = _two_cliques()
    runs = [ur.pagerank(edges).collect().to_dicts() for _ in range(5)]
    for r in runs[1:]:
        assert r == runs[0]  # exact f64 equality, not approximate


def test_weighted_pagerank_is_bit_reproducible():
    tbl = pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0], "w": [1.0, 9.0, 1.0, 1.0]})
    edges = ur.from_arrow(tbl, src="s", dst="d")
    runs = [
        edges.nodes().with_columns(pr=ur.pagerank(edges, weight=ur.col("w"))).collect().to_dicts()
        for _ in range(5)
    ]
    for r in runs[1:]:
        assert r == runs[0]


# --- #40.1: Louvain is reproducible under same seed ------------------------
def test_louvain_is_reproducible():
    edges = _two_cliques()
    runs = [
        {r["id"]: r["louvain"] for r in ur.louvain(edges, seed=1).collect().to_dicts()}
        for _ in range(5)
    ]
    for r in runs[1:]:
        assert r == runs[0]


# --- #40.3: sampled betweenness takes a reproducible seed ------------------
def test_betweenness_sample_seed_is_reproducible():
    src = [0, 1, 2, 3, 4, 5, 6, 0, 2, 4]
    dst = [1, 2, 3, 4, 5, 6, 0, 3, 5, 6]
    edges = ur.from_arrow(pa.table({"s": src, "d": dst}), src="s", dst="d")
    a = ur.betweenness(edges, sample=0.5, seed=7).collect().to_dicts()
    b = ur.betweenness(edges, sample=0.5, seed=7).collect().to_dicts()
    assert a == b  # same seed -> identical sampled estimate


def test_betweenness_exact_unaffected_by_seed():
    # exact (sample=None) ignores seed and matches the known directed-line values
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2, 3], "d": [1, 2, 3, 4]}), src="s", dst="d")
    got = {r["id"]: r["betweenness"] for r in ur.betweenness(edges, seed=99).collect().to_dicts()}
    assert got == {0: 0.0, 1: 3.0, 2: 4.0, 3: 3.0, 4: 0.0}


# --- #37: weighted over a glob matches a single file -----------------------
def test_weighted_pagerank_glob_matches_single_file(tmp_path):
    src = [0, 0, 1, 2, 2, 3, 3, 4, 4, 0]
    dst = [1, 2, 0, 0, 3, 4, 0, 3, 0, 4]
    w = [1.0, 9.0, 1.0, 1.0, 2.0, 5.0, 1.0, 3.0, 1.0, 7.0]
    full = pa.table({"s": src, "d": dst, "w": w})

    single = tmp_path / "single.parquet"
    pq.write_table(full, single)

    parts_dir = tmp_path / "parts"
    parts_dir.mkdir()
    # split into three parts so a multi-partition scan is exercised
    for i, lo in enumerate((0, 4, 7)):
        hi = (4, 7, len(src))[i]
        pq.write_table(full.slice(lo, hi - lo), parts_dir / f"part-{i}.parquet")

    def weighted_pr(path):
        edges = ur.scan_edges(path, src="s", dst="d")
        rows = (
            edges.nodes()
            .with_columns(pr=ur.pagerank(edges, weight=ur.col("w")))
            .collect()
            .to_dicts()
        )
        return {r["id"]: r["pr"] for r in rows}

    from_single = weighted_pr(str(single))
    from_glob = weighted_pr(str(parts_dir / "*.parquet"))
    assert from_single.keys() == from_glob.keys()
    for k in from_single:
        assert abs(from_single[k] - from_glob[k]) < 1e-9
