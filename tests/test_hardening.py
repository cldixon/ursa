"""Panic-surface hardening (#39): valid-but-unusual input surfaces as a catchable
exception, not a Rust panic that crosses PyO3 as a BaseException.

Using ``pytest.raises(Exception, ...)`` is the assertion: a PanicException is a
BaseException, so if any of these still panicked the test would error rather than
pass. (The kernel/id-map overflow guards and LargeUtf8 handling are unit-tested in
Rust; a 2 GiB / 4.29B-node input is unreasonable to allocate in CI.)
"""

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges():
    return ur.from_arrow(pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0]}), src="s", dst="d")


def test_reserved_output_column_name_is_a_clean_error():
    # 'id' is the node id column; using it as an output name would collide and
    # (before the fix) panic inside the node constructor's DFSchema conversion.
    edges = _edges()
    with pytest.raises(Exception, match="reserved"):
        edges.nodes().with_columns(id=ur.degree(edges)).collect()


def test_string_weight_column_reports_numeric_not_null():
    # A non-numeric weight column must say "numeric", not the misleading "null".
    edges = ur.from_arrow(
        pa.table({"s": [0, 1], "d": [1, 0], "region": ["us", "eu"]}), src="s", dst="d"
    )
    with pytest.raises(Exception, match="numeric"):
        edges.nodes().with_columns(pr=ur.pagerank(edges, weight=ur.col("region"))).collect()
