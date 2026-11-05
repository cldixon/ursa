"""A weighted algorithm over a scan reads the file once, evaluating its weights a
batch at a time as the index is built, so the rows are never held (#267).

The results are those of the same rows in memory, bit for bit; the errors and
the ``on_null="drop"`` warning are as they were.
"""

from __future__ import annotations

import warnings

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa._frames import _native

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _table(n: int = 3000) -> pa.Table:
    src = [(i * 7) % 211 for i in range(n)]
    dst = [(i * 13 + 5) % 211 for i in range(n)]
    w = [((i * 31) % 97) / 7.0 for i in range(n)]
    return pa.table({"src": src, "dst": dst, "w": w, "tag": [str(i % 3) for i in range(n)]})


@pytest.fixture
def scan(tmp_path):
    t = _table()
    path = tmp_path / "e.parquet"
    # Small row groups, so the scan streams many batches.
    pq.write_table(t, path, row_group_size=256)
    return t, str(path)


def _pagerank(edges, weight):
    return ur.pagerank(edges, weight=weight).collect().to_arrow()


def test_streamed_weights_match_the_rows_in_memory(scan):
    t, path = scan
    for weight in (ur.col("w"), ur.col("w") * 2 + 1):
        mem = _pagerank(ur.from_arrow(t, src="src", dst="dst"), weight)
        got = _pagerank(ur.scan_edges(path, src="src", dst="dst"), weight)
        assert got.equals(mem)


def test_two_weights_in_one_collect_read_the_file_once(scan):
    t, path = scan
    edges = ur.scan_edges(path, src="src", dst="dst")
    _native()._scan_log(True)
    out = (
        edges.nodes()
        .with_columns(
            a=ur.pagerank(edges, weight=ur.col("w")),
            b=ur.degree(edges),
            c=ur.pagerank(edges, weight=ur.col("w") + 1),
        )
        .collect()
        .to_arrow()
    )
    log = _native()._scan_log(False)
    edge_reads = [cols for kind, cols, _ in log if kind in ("edges", "index")]
    assert edge_reads == [["w"]]
    mem = ur.from_arrow(t, src="src", dst="dst")
    want = (
        mem.nodes()
        .with_columns(
            a=ur.pagerank(mem, weight=ur.col("w")),
            b=ur.degree(mem),
            c=ur.pagerank(mem, weight=ur.col("w") + 1),
        )
        .collect()
        .to_arrow()
    )
    assert out.equals(want)


def test_more_weights_than_the_cache_keeps_still_agree(scan):
    t, path = scan
    weights = [ur.col("w") + k for k in range(3)]

    def run(edges):
        cols = {f"p{k}": ur.pagerank(edges, weight=w) for k, w in enumerate(weights)}
        return edges.nodes().with_columns(**cols).collect().to_arrow()

    got = run(ur.scan_edges(path, src="src", dst="dst"))
    assert got.equals(run(ur.from_arrow(t, src="src", dst="dst")))


def test_a_bad_weight_raises_as_before(scan):
    _, path = scan
    edges = ur.scan_edges(path, src="src", dst="dst")
    with pytest.raises(ur.ComputeError, match="weight expression must be numeric"):
        _pagerank(edges, ur.col("tag"))
    with pytest.raises(ur.ComputeError, match="negative value"):
        _pagerank(edges, ur.col("w") - 100)
    with pytest.raises(ur.ColumnNotFoundError):
        _pagerank(edges, ur.col("nope"))
    # The frame still works after a failed weight.
    assert _pagerank(edges, ur.col("w")).num_rows == 211


def test_on_null_drop_warns_once_and_matches_memory(tmp_path):
    t = _table()
    src = t.column("src").to_pylist()
    src[5] = src[700] = None
    t = t.set_column(0, "src", pa.array(src, pa.int64()))
    path = tmp_path / "e.parquet"
    pq.write_table(t, path, row_group_size=256)
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        got = _pagerank(ur.scan_edges(str(path), src="src", dst="dst", on_null="drop"), ur.col("w"))
    assert [str(w.message) for w in caught] == [
        "on_null='drop': dropped 2 edge row(s) with a null src or dst endpoint."
    ]
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        mem = _pagerank(ur.from_arrow(t, src="src", dst="dst", on_null="drop"), ur.col("w"))
    assert got.equals(mem)


def test_a_null_endpoint_still_raises_without_drop(tmp_path):
    t = _table()
    src = t.column("src").to_pylist()
    src[5] = None
    t = t.set_column(0, "src", pa.array(src, pa.int64()))
    path = tmp_path / "e.parquet"
    pq.write_table(t, path)
    edges = ur.scan_edges(str(path), src="src", dst="dst")
    with pytest.raises(Exception, match="null"):
        _pagerank(edges, ur.col("w"))
