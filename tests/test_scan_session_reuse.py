"""Scan sessions are reused across collects (#175), with DataFusion's Parquet
footer and statistics caches on for local files. A file rewritten in place, or a
glob that gains a file, must still read the new data on the next scan.
"""

import os
import time

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _sources(path):
    return ur.scan_edges(str(path), src="s", dst="d").collect().to_arrow().column("s").to_pylist()


def test_a_parquet_file_rewritten_in_place_is_read_afresh(tmp_path):
    path = tmp_path / "edges.parquet"
    pq.write_table(pa.table({"s": [1, 2], "d": [2, 3]}), path)
    assert _sources(path) == [1, 2]
    assert ur.degree(ur.scan_edges(str(path), src="s", dst="d")).collect().to_arrow().num_rows == 3

    # A different size.
    pq.write_table(pa.table({"s": [7, 8, 9], "d": [8, 9, 7]}), path)
    assert _sources(path) == [7, 8, 9]

    # The same size: only the modification time differs. Move it forward
    # explicitly so a coarse filesystem clock cannot hide the rewrite.
    size = path.stat().st_size
    pq.write_table(pa.table({"s": [4, 5, 6], "d": [5, 6, 4]}), path)
    assert path.stat().st_size == size
    later = time.time() + 5
    os.utime(path, (later, later))
    assert _sources(path) == [4, 5, 6]


def test_a_glob_sees_a_file_added_between_scans(tmp_path):
    pq.write_table(pa.table({"s": [1], "d": [2]}), tmp_path / "part-0.parquet")
    glob = tmp_path / "*.parquet"
    assert _sources(glob) == [1]
    pq.write_table(pa.table({"s": [3], "d": [4]}), tmp_path / "part-1.parquet")
    assert _sources(glob) == [1, 3]
