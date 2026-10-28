"""NaN in a Parquet float column reads back as NaN (#202).

Parquet min/max statistics leave NaN out, and DataFusion 54 marked a float
column's file-level bounds exact. A float column whose non-NaN values were all
equal (for instance an all-NaN row group beside a constant one) was then read as
that constant, NaN rows included. Each scan is checked against ``pq.read_table``
for the rows, and against the in-memory frame over the same table for filters
and graph ops.
"""

import math

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

NAN = math.nan

# (values, row_group_size): the issue's case, and its neighbours.
CASES = {
    "all_nan_group_beside_constant": ([NAN, NAN, 1.0], 2),
    "nan_beside_constant_one_group": ([NAN, 1.0, 1.0], None),
    "all_nan": ([NAN, NAN, NAN], 2),
    "all_null_group_beside_constant": ([None, None, 1.0], 2),
    "mixed_groups": ([NAN, None, 1.0, 1.0, NAN, 2.0], 2),
    "issue_control": ([NAN, 2.0, 1.0], 2),
}
TYPES = {"float32": pa.float32(), "float64": pa.float64()}


def _table(values, dtype):
    n = len(values)
    return pa.table(
        {
            "s": list(range(n)),
            "d": [(i + 1) % n for i in range(n)],
            "r": pa.array(values, type=dtype),
        }
    )


def _write(tmp_path, values, dtype, group):
    path = tmp_path / "e.parquet"
    pq.write_table(_table(values, dtype), path, row_group_size=group)
    return str(path)


def _key(values):
    """NaN-aware values, so NaN compares equal to NaN."""
    return [("nan" if isinstance(v, float) and math.isnan(v) else v) for v in values]


def _col(frame, name="r"):
    return _key(frame.collect().to_arrow().column(name).to_pylist())


@pytest.fixture(params=list(CASES), ids=list(CASES))
def case(request):
    return CASES[request.param]


@pytest.fixture(params=list(TYPES), ids=list(TYPES))
def dtype(request):
    return TYPES[request.param]


def test_rows_read_back_as_written(tmp_path, case, dtype):
    values, group = case
    path = _write(tmp_path, values, dtype, group)
    want = pq.read_table(path)
    got = ur.scan_edges(path, src="s", dst="d").collect().to_arrow()
    assert got.schema.field("r").type == dtype
    assert _key(got.column("r").to_pylist()) == _key(want.column("r").to_pylist())
    assert _key(want.column("r").to_pylist()) == _key(values)


def test_issue_repro(tmp_path):
    path = _write(tmp_path, [NAN, NAN, 1.0], pa.float64(), 2)
    assert _col(ur.scan_edges(path, src="s", dst="d")) == ["nan", "nan", 1.0]


@pytest.mark.parametrize(
    "pred",
    [
        pytest.param(lambda: ur.col("r") > 1, id="gt_1"),
        pytest.param(lambda: ur.col("r") == NAN, id="eq_nan"),
        pytest.param(lambda: ur.col("r") < 2, id="lt_2"),
    ],
)
def test_a_filter_on_the_nan_column_matches_the_in_memory_frame(tmp_path, case, dtype, pred):
    values, group = case
    path = _write(tmp_path, values, dtype, group)
    scan = ur.scan_edges(path, src="s", dst="d").filter(pred())
    mem = ur.from_arrow(pq.read_table(path), src="s", dst="d").filter(pred())
    assert _col(scan, "s") == _col(mem, "s")
    assert _col(scan) == _col(mem)


def test_nan_filters_follow_total_order(tmp_path, dtype):
    # Total order (#199): NaN equals NaN and is above every number.
    path = _write(tmp_path, [NAN, NAN, 1.0], dtype, 2)
    scan = ur.scan_edges(path, src="s", dst="d")
    assert _col(scan.filter(ur.col("r") == NAN), "s") == [0, 1]
    assert _col(scan.filter(ur.col("r") > 1), "s") == [0, 1]
    assert _col(scan.filter(ur.col("r") == 1), "s") == [2]


def test_weighted_pagerank_over_an_all_nan_row_group(tmp_path, dtype):
    # The weight column's first row group is all NaN; the rest is the constant
    # 1.0. Read as the constant, every weight was 1.0.
    values = [NAN, NAN, 1.0, 1.0, 1.0, 1.0]
    table = pa.table(
        {
            "s": [0, 1, 2, 3, 0, 2],
            "d": [1, 2, 3, 0, 2, 0],
            "r": pa.array(values, type=dtype),
        }
    )
    path = tmp_path / "w.parquet"
    pq.write_table(table, path, row_group_size=2)

    def ranks(e):
        out = e.nodes().with_columns(pr=ur.pagerank(e, weight=ur.col("r"))).collect()
        return {r["id"]: r["pr"] for r in out.to_dicts()}

    scan = ranks(ur.scan_edges(str(path), src="s", dst="d"))
    mem = ranks(ur.from_arrow(table, src="s", dst="d"))
    assert scan == mem
    # Not the graph the bug read: the same edges, every weight 1.0.
    ones = table.set_column(2, "r", pa.array([1.0] * 6, type=dtype))
    assert scan != ranks(ur.from_arrow(ones, src="s", dst="d"))
