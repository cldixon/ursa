"""Streaming RecordBatches across the FFI (#60).

A scan large enough that DataFusion emits **many** batches must ingest, compute,
and egress identically to a single contiguous build — proving the topology build,
the weight evaluation, and the result egress all stream batch-by-batch (never
`concat_batches` into one) without changing a single number.

The comparison is scan (multi-batch) vs the same table in memory (one build): if
any streaming step misaligned a batch, the two would diverge.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# Comfortably above DataFusion's 8192-row default batch size, so the parquet scan
# is delivered — and consumed — as several RecordBatches rather than one.
N = 20_000


def _ring_with_chords() -> pa.Table:
    """A deterministic directed graph on ``N`` nodes (two out-edges each) with a
    per-edge weight column. Structured enough that PageRank is non-trivial."""
    src: list[int] = []
    dst: list[int] = []
    weight: list[float] = []
    for i in range(N):
        for j, mul, add in ((0, 7, 3), (1, 13, 1)):
            src.append(i)
            dst.append((i * mul + add) % N)
            weight.append(1.0 + ((i + j) % 5))
    return pa.table(
        {
            "src": pa.array(src, pa.int64()),
            "dst": pa.array(dst, pa.int64()),
            "weight": pa.array(weight, pa.float64()),
        }
    )


def _sorted_pairs(mf) -> list[tuple[int, float]]:
    rows = mf.collect().to_dicts()
    key = "pr" if "pr" in rows[0] else "pagerank"
    return sorted((r["id"], r[key]) for r in rows)


def test_multi_batch_scan_matches_inmemory_pagerank(tmp_path):
    tbl = _ring_with_chords()
    path = tmp_path / "edges.parquet"
    pq.write_table(tbl, path)

    scanned = _sorted_pairs(ur.pagerank(ur.scan_edges(str(path), src="src", dst="dst")))
    inmem = _sorted_pairs(ur.pagerank(ur.from_arrow(tbl, src="src", dst="dst")))

    assert [i for i, _ in scanned] == [i for i, _ in inmem]
    for (_, a), (_, b) in zip(scanned, inmem, strict=True):
        assert abs(a - b) < 1e-9


def test_multi_batch_weighted_scan_stays_aligned(tmp_path):
    # The weight f64 array is concatenated across the scan's batches; if that order
    # drifted from the topology's edge_ids, the multi-batch scan and the one-build
    # in-memory table would give different weighted PageRanks.
    tbl = _ring_with_chords()
    path = tmp_path / "edges.parquet"
    pq.write_table(tbl, path)

    scan_edges = ur.scan_edges(str(path), src="src", dst="dst")
    scanned = _sorted_pairs(
        scan_edges.nodes().with_columns(pr=ur.pagerank(scan_edges, weight=ur.col("weight")))
    )
    mem_edges = ur.from_arrow(tbl, src="src", dst="dst")
    inmem = _sorted_pairs(
        mem_edges.nodes().with_columns(pr=ur.pagerank(mem_edges, weight=ur.col("weight")))
    )

    assert [i for i, _ in scanned] == [i for i, _ in inmem]
    for (_, a), (_, b) in zip(scanned, inmem, strict=True):
        assert abs(a - b) < 1e-9


def test_multi_batch_scan_plain_collect_roundtrips_all_rows(tmp_path):
    # Egress: the plain edge collect assembles the scan's batch list into a chunked
    # Table — every one of the 2N edges must come back.
    tbl = _ring_with_chords()
    path = tmp_path / "edges.parquet"
    pq.write_table(tbl, path)

    out = ur.scan_edges(str(path), src="src", dst="dst").to_arrow()
    assert out.num_rows == 2 * N
