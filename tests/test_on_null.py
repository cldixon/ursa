"""on_null="drop" — opt-in filtering of null-endpoint edge rows (issue #71).

A null ``src``/``dst`` endpoint errors by default (the safe null policy). Passing
``on_null="drop"`` at the edge-ingest boundary (``from_arrow`` / ``from_edgelist`` /
``scan_edges`` / ``read_edges`` / ``EdgeFrame(...)``) instead filters those rows out
before the topology is built, emitting a count so a silent drop can't masquerade as
"all rows ingested". ``"error"`` stays the default.
"""

from __future__ import annotations

import warnings

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges_with_nulls():
    # rows 2 and 3 each carry a null endpoint; rows 0 and 1 are clean.
    return pa.table({"s": [0, 1, None, 2], "d": [1, 3, 0, None]})


# --- in-memory -------------------------------------------------------------


def test_in_memory_drop_filters_null_endpoints():
    ef = ur.from_arrow(_edges_with_nulls(), src="s", dst="d", on_null="drop")
    out = ef.collect().to_arrow()
    assert out.num_rows == 2  # the two null-endpoint rows are gone
    assert out.column("s").to_pylist() == [0, 1]
    assert out.column("d").to_pylist() == [1, 3]


def test_in_memory_drop_warns_with_count():
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        ur.from_arrow(_edges_with_nulls(), src="s", dst="d", on_null="drop")
    assert len(caught) == 1
    assert "dropped 2" in str(caught[0].message)


def test_in_memory_default_errors_on_graph_op():
    # The default policy leaves nulls in place; the topology build (a graph op)
    # then raises. (A plain collect of edges never builds the topology, so the
    # error surfaces at the first graph op, not at construction.)
    ef = ur.from_arrow(_edges_with_nulls(), src="s", dst="d")
    with pytest.raises(Exception):  # noqa: B017
        ur.pagerank(ef).collect()


def test_no_nulls_drop_is_a_noop_and_silent():
    clean = pa.table({"s": [0, 1, 2], "d": [1, 2, 0]})
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        out = ur.from_arrow(clean, src="s", dst="d", on_null="drop").collect().to_arrow()
    assert out.num_rows == 3
    assert caught == []  # nothing dropped -> no warning


def test_invalid_on_null_value_errors_at_construction():
    with pytest.raises(ValueError, match="on_null"):
        ur.from_arrow(_edges_with_nulls(), src="s", dst="d", on_null="skip")


def test_edgeframe_constructor_accepts_on_null():
    ef = ur.EdgeFrame(_edges_with_nulls(), src="s", dst="d", on_null="drop")
    assert ef.collect().to_arrow().num_rows == 2


def test_from_edgelist_drop():
    # A hand-written edge list with a null endpoint tuple.
    ef = ur.from_edgelist([(0, 1), (1, 2), (None, 3)], on_null="drop")
    assert ef.collect().to_arrow().num_rows == 2


# --- weighted alignment ----------------------------------------------------


def test_weighted_algo_stays_aligned_after_drop():
    # The dropped row carries a huge weight; if the drop misaligned endpoints and
    # weights, that weight would corrupt the result. Filtering the whole table at
    # construction keeps endpoints and weights row-aligned.
    t = pa.table({"s": [0, 1, None], "d": [1, 0, 2], "w": [1.0, 1.0, 99.0]})
    ef = ur.from_arrow(t, src="s", dst="d", on_null="drop")
    out = ef.nodes().with_columns(p=ur.pagerank(ef, weight=ur.col("w"))).collect().to_arrow()
    assert out.num_rows == 2  # only nodes 0 and 1 remain
    # A 0<->1 two-cycle with equal weights is symmetric: both ranks equal.
    p = dict(zip(out.column("id").to_pylist(), out.column("p").to_pylist(), strict=True))
    assert abs(p[0] - p[1]) < 1e-9


# --- scan ------------------------------------------------------------------


def test_scan_drop_filters_null_endpoints(tmp_path):
    path = tmp_path / "e.parquet"
    pq.write_table(_edges_with_nulls(), path)
    out = ur.scan_edges(str(path), src="s", dst="d", on_null="drop").collect().to_arrow()
    assert out.num_rows == 2


def test_scan_drop_warns(tmp_path):
    path = tmp_path / "e.parquet"
    pq.write_table(_edges_with_nulls(), path)
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        ur.scan_edges(str(path), src="s", dst="d", on_null="drop").collect()
    assert any("dropped 2" in str(c.message) for c in caught)


def test_scan_default_errors_on_graph_op(tmp_path):
    path = tmp_path / "e.parquet"
    pq.write_table(_edges_with_nulls(), path)
    with pytest.raises(Exception):  # noqa: B017
        ur.pagerank(ur.scan_edges(str(path), src="s", dst="d")).collect()


def test_scan_weighted_stays_aligned_after_drop(tmp_path):
    t = pa.table({"s": [0, 1, None], "d": [1, 0, 2], "w": [1.0, 1.0, 99.0]})
    path = tmp_path / "w.parquet"
    pq.write_table(t, path)
    ef = ur.scan_edges(str(path), src="s", dst="d", on_null="drop")
    out = ef.nodes().with_columns(p=ur.pagerank(ef, weight=ur.col("w"))).collect().to_arrow()
    assert out.num_rows == 2
    p = dict(zip(out.column("id").to_pylist(), out.column("p").to_pylist(), strict=True))
    assert abs(p[0] - p[1]) < 1e-9


def test_read_edges_threads_on_null(tmp_path):
    # read_edges is scan_edges(...).collect(); on_null flows through **kwargs.
    path = tmp_path / "e.parquet"
    pq.write_table(_edges_with_nulls(), path)
    out = ur.read_edges(str(path), src="s", dst="d", on_null="drop").to_arrow()
    assert out.num_rows == 2
