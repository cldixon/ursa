"""One expression evaluator (#192): every filter, subgraph mask and plain-source
read runs in Rust.

These pin what moved: traversals honour a filtered edge frame (the one behaviour
fix), the NaN semantics each path keeps, typed literals, the error type a kernel
type mismatch raises, scan pushdown, and the Rust-side caches.
"""

import datetime
import decimal
import math

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa._execute import _native

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# 0 -> 1 -> 2 -> 3, plus 0 -> 2 and 3 -> 0; `w` marks the edges a filter keeps.
EDGES = pa.table(
    {
        "s": [0, 1, 2, 0, 3],
        "d": [1, 2, 3, 2, 0],
        "w": [1.0, 5.0, 5.0, 1.0, 5.0],
    }
)


def _edges():
    return ur.from_arrow(EDGES, src="s", dst="d")


def _rows(frame):
    return frame.collect().to_arrow().to_pylist()


# --- traversals over a filtered edge frame (#192, a fix) -------------------------
def test_hop_over_a_filtered_frame_follows_only_kept_edges():
    view = _edges().filter(ur.col("w") > 2)  # keeps 1->2, 2->3, 3->0
    got = _rows(ur.hop(view, n=3).from_([0]))
    # 0's only kept out-edge is gone (0->1 and 0->2 have w=1): nothing is reached.
    assert got == []
    assert _rows(ur.hop(view, n=2).from_([1])) == [
        {"src": 1, "dst": 2},
        {"src": 1, "dst": 3},
    ]
    # The unfiltered frame still reaches through every edge.
    assert len(_rows(ur.hop(_edges(), n=3).from_([0]))) == 3


def test_shortest_path_and_random_walk_honour_the_filter():
    view = _edges().filter(ur.col("w") > 2)
    path = _rows(ur.shortest_path(view, 1, 0))
    assert [(r["src"], r["dst"]) for r in path] == [(1, 2), (2, 3), (3, 0)]
    # 0 -> 2 is filtered out, so the direct route is gone.
    assert _rows(ur.shortest_path(view, 0, 2)) == []
    walk = _rows(ur.random_walk(view, start=[1], steps=5, seed=3))
    assert [r["node"] for r in walk] == [1, 2, 3, 0]  # stops: 0 has no kept out-edge


def test_a_graph_op_over_a_traversal_of_a_view_stays_in_the_view():
    view = _edges().filter(ur.col("w") > 2)
    reached = ur.hop(view, n=1).from_([2, 3])  # reaches {2, 3, 0}
    degree = {r["id"]: r["degree"] for r in _rows(ur.degree(reached)._frame())}
    # Induced on {0, 2, 3} within the view: 2->3 and 3->0 (0->2 is filtered out).
    assert degree == {0: 0, 1: 0, 2: 1, 3: 1}


def test_traversal_masks_are_cached_on_the_index():
    edges = _edges()
    view = edges.filter(ur.col("w") > 2)
    reached = ur.hop(view, n=1).from_([2])
    first = ur.degree(reached).collect().to_arrow()
    builds = edges._index_build_cell.value._mask_build_count()
    assert builds == 2  # the filter's mask, and the traversal's within it
    assert ur.degree(reached).collect().to_arrow().equals(first)
    assert edges._index_build_cell.value._mask_build_count() == builds


def test_equal_views_share_one_mask():
    # The mask cache is keyed by the view, so two frames with the same filter over
    # one index compute it once.
    edges = _edges()
    a = ur.degree(edges.filter(ur.col("w") > 2)).collect().to_arrow()
    b = ur.degree(edges.filter(ur.col("w") > 2)).collect().to_arrow()
    assert a.equals(b)
    assert edges._index_build_cell.value._mask_build_count() == 1


# --- NaN: total order on every path (#199, a behaviour change) --------------------
NAN_TABLE = pa.table(
    {"id": [0, 1, 2, 3, 4], "f": pa.array([0.5, math.nan, None, 2.0, -1.0], pa.float64())}
)


def _plain(pred):
    return _rows(ur.from_arrow(NAN_TABLE, id="id").filter(pred).select("id"))


def _engine(pred):
    keys = ur.from_arrow(pa.table({"id": [0, 1, 2, 3, 4]}), id="id")
    return _rows(ur.from_arrow(NAN_TABLE, id="id").join(keys, on="id").filter(pred).select("id"))


def _mask(pred):
    edges = ur.from_arrow(
        NAN_TABLE.append_column("d", pa.array([1, 2, 3, 4, 0])), src="id", dst="d"
    )
    out = _rows(ur.degree(edges.filter(pred))._frame())
    return [{"id": r["id"]} for r in out if r["degree"]]


# Rows kept, ids into f = [0.5, NaN, null, 2.0, -1.0]. The comment is what main
# kept on plain frames and masks, which compared IEEE-style (the engine already
# used total order).
@pytest.mark.parametrize(
    ("pred", "kept"),
    [
        (ur.col("f") > 1.0, [1, 3]),  # main: [3]
        (~(ur.col("f") > 1.0), [0, 4]),  # main: [0, 1, 4]
        (ur.col("f") != ur.col("f"), []),  # main: [1]
        ((ur.col("f") / 0) > 1, [0, 1, 3]),  # main: [0, 3]
        (ur.col("f") == math.nan, [1]),  # main: []
        (ur.col("f") != math.nan, [0, 3, 4]),  # main: [0, 1, 3, 4]
        (ur.col("f") < math.inf, [0, 3, 4]),  # unchanged
    ],
)
def test_nan_now_compares_in_total_order_on_plain_and_mask_paths(pred, kept):
    # NaN equals NaN and is above every number, as in sort() and inside the engine.
    assert [r["id"] for r in _plain(pred)] == kept
    assert [r["id"] for r in _engine(pred)] == kept
    assert [r["id"] for r in _mask(pred)] == kept


# --- typed literals ---------------------------------------------------------------
TYPED = pa.table(
    {
        "id": [0, 1, 2],
        "dec": pa.array([decimal.Decimal(x) for x in ("1.5", "2.5", "0.1")]),
        "dt": pa.array([datetime.date(2020, 1, d) for d in (1, 2, 3)]),
        "tm": pa.array([datetime.time(h) for h in (1, 2, 3)]),
        "i": [1, None, 3],
    }
)


@pytest.mark.parametrize(
    ("pred", "kept"),
    [
        (ur.col("dec") > decimal.Decimal("1.0"), [0, 1]),
        (ur.col("dt") >= datetime.date(2020, 1, 2), [1, 2]),
        (ur.col("tm") < datetime.time(2), [0]),
        (ur.col("i") == None, []),  # noqa: E711 - a null literal keeps nothing
    ],
)
def test_typed_literals_filter_the_same_rows_on_every_path(pred, kept):
    nodes = ur.from_arrow(TYPED, id="id")
    edges = ur.from_arrow(TYPED.append_column("d", pa.array([1, 2, 0])), src="id", dst="d")
    assert [r["id"] for r in _rows(nodes.filter(pred).select("id"))] == kept
    attrs = nodes.with_columns(dg=ur.degree(edges))  # an engine filter
    assert [r["id"] for r in _rows(attrs.filter(pred).select("id"))] == kept
    out = {r["id"]: r["degree"] for r in _rows(ur.degree(edges.filter(pred))._frame())}
    assert [k for k, v in sorted(out.items()) if v] == kept  # a subgraph mask


def test_a_kernel_type_mismatch_is_a_compute_error():
    # main raised pyarrow.ArrowNotImplementedError on plain frames; the engine path
    # raised ComputeError for the same predicate, which is now what both raise.
    nodes = ur.from_arrow(TYPED, id="id")
    with pytest.raises(
        ur.ComputeError,
        match=r"Function 'equal' has no kernel matching input types \(int64, string\)",
    ):
        nodes.filter(ur.col("i") == "x").collect()
    with pytest.raises(
        ur.ColumnNotFoundError, match=r"filter\(\) references unknown column 'nope'"
    ):
        nodes.filter(ur.col("nope") > 1).collect()


# --- scans --------------------------------------------------------------------------
@pytest.fixture
def scan_log():
    _native()._scan_log(True)
    yield lambda: _native()._scan_log(True)
    _native()._scan_log(False)


def _write_groups(path, n=40_000):
    t = pa.table({"s": list(range(n)), "d": [(i + 1) % n for i in range(n)], "w": list(range(n))})
    pq.write_table(t, path, row_group_size=1_000)
    return t


def test_a_plain_scan_takes_its_leading_filters(tmp_path, scan_log):
    path = tmp_path / "e.parquet"
    table = _write_groups(path)
    scan = ur.scan_edges(str(path), src="s", dst="d")
    pred = (ur.col("w") >= 39_500) & (ur.src() != 39_990)
    got = scan.filter(pred).sort("w", descending=True).head(3).collect().to_arrow()
    assert scan_log() == [("table", [], 1)]  # one read, the filter pushed into it
    want = ur.from_arrow(table, src="s", dst="d").filter(pred).sort("w", descending=True).head(3)
    assert got.equals(want.collect().to_arrow())
    # A filter after another step stays after it.
    scan.head(10).filter(ur.col("w") > 5).collect()
    assert scan_log() == [("table", [], 0)]


def test_on_null_drop_counts_the_whole_file_before_filtering(tmp_path):
    t = pa.table({"s": [0, None, 1, 2], "d": [1, 2, None, 0], "w": [1.0, 9.0, 9.0, 1.0]})
    path = tmp_path / "e.parquet"
    pq.write_table(t, path)
    scan = ur.scan_edges(str(path), src="s", dst="d", on_null="drop")
    with pytest.warns(UserWarning, match="dropped 2 edge row"):
        out = scan.filter(ur.col("w") < 5).collect().to_arrow()
    assert out.column("s").to_pylist() == [0, 2]


def test_a_scanned_node_table_keeps_one_superset_per_frame(tmp_path):
    path = tmp_path / "n.parquet"
    pq.write_table(pa.table({"id": [0, 1, 2], "a": [1, 2, 3], "b": ["x", "y", "z"]}), path)
    nodes = ur.scan_nodes(str(path), id="id")
    nodes.select("id", "a").collect()
    cache = nodes._attr_build_cell.value
    assert cache is not None
    assert cache._held() == ["a", "id"]
    nodes.filter(ur.col("a") > 1).select("id").collect()  # a subset: no read
    assert cache._scan_count() == 1
    nodes.select("id", "b").collect()  # widens to the union, once
    assert sorted(cache._held()) == ["a", "b", "id"]
    assert cache._scan_count() == 2
    derived = nodes.filter(ur.col("b") != "x")  # derivations share the cache
    assert _rows(derived.select("id", "a", "b")) == [
        {"id": 1, "a": 2, "b": "y"},
        {"id": 2, "a": 3, "b": "z"},
    ]
    assert derived._attr_build_cell.value is cache
    assert cache._scan_count() == 2


# --- timezone-aware vs naive timestamps stay an error ----------------------------
TZ = pa.table(
    {
        "id": [0, 1, 2],
        "d": [1, 2, 0],
        "utc": pa.array(
            [datetime.datetime(2020, 1, d) for d in (1, 2, 3)], pa.timestamp("us", "UTC")
        ),
        "naive": pa.array([datetime.datetime(2020, 1, d) for d in (1, 2, 3)], pa.timestamp("us")),
    }
)
NAIVE = datetime.datetime(2020, 1, 2)
AWARE = datetime.datetime(2020, 1, 2, tzinfo=datetime.timezone.utc)


@pytest.mark.parametrize(
    ("pred", "message"),
    [
        (
            ur.col("utc") > NAIVE,
            r"Cannot compare timestamp with timezone to timestamp without timezone, "
            r"got: timestamp\[us, tz=UTC\] and timestamp\[us\]",
        ),
        (
            ur.col("naive") > AWARE,
            r"Cannot compare timestamp with timezone to timestamp without timezone, "
            r"got: timestamp\[us\] and timestamp\[us, tz=UTC\]",
        ),
    ],
)
@pytest.mark.parametrize("path", ["plain", "mask", "engine"])
def test_an_aware_naive_timestamp_comparison_is_a_value_error(pred, message, path):
    # main raised pyarrow's ArrowInvalid (a ValueError) here; DataFusion alone
    # would read the naive side as UTC.
    nodes = ur.from_arrow(TZ, id="id")
    edges = ur.from_arrow(TZ, src="id", dst="d")
    build = {
        "plain": lambda: nodes.filter(pred),
        "mask": lambda: ur.degree(edges.filter(pred))._frame(),
        "engine": lambda: nodes.with_columns(dg=ur.degree(edges)).filter(pred),
    }[path]
    # The types are known from the in-memory tables, so the filter is rejected
    # where it is added (#193).
    with pytest.raises(ValueError, match=message):
        build()
    # The same instant on both sides compares.
    same = ur.col("utc") > AWARE
    assert [r["id"] for r in _rows(nodes.filter(same).select("id"))] == [2]


# --- comparisons resolve as pyarrow's kernels did, on every path (#192 review) ---
NEG_NAN = float((np.array([0.0]) / np.array([0.0]))[0])  # 0/0: the sign bit is set on x86


def _table(**cols):
    n = len(next(iter(cols.values())))
    return pa.table({"id": list(range(n)), "d": [(i + 1) % n for i in range(n)], **cols})


def _every_path(table, pred, tmp_path):
    """The ids `pred` keeps (or the error it raises) on the plain, scan (pushdown),
    mask, mask-over-scan and engine paths, which must all agree."""
    path = tmp_path / "t.parquet"
    pq.write_table(table, path, row_group_size=2)
    nodes = ur.from_arrow(table, id="id")
    edges = ur.from_arrow(table, src="id", dst="d")
    scan = ur.scan_edges(str(path), src="id", dst="d")
    keys = ur.from_arrow(pa.table({"id": table.column("id")}), id="id")

    def masked(e):
        out = _rows(ur.degree(e.filter(pred))._frame())
        return sorted(r["id"] for r in out if r["degree"])

    paths = {
        "plain": lambda: sorted(r["id"] for r in _rows(nodes.filter(pred))),
        "scan": lambda: sorted(r["id"] for r in _rows(scan.filter(pred))),
        "mask": lambda: masked(edges),
        "scan_mask": lambda: masked(scan),
        "engine": lambda: sorted(r["id"] for r in _rows(nodes.join(keys, on="id").filter(pred))),
    }
    out = {}
    for name, run in paths.items():
        try:
            out[name] = run()
        except Exception as e:  # the error class and message are compared too
            out[name] = (type(e).__name__, str(e))
    assert len({repr(v) for v in out.values()}) == 1, out
    return out["plain"]


TS_NS = pa.array([1500], pa.timestamp("ns"))  # 1.5 us after the epoch
ONE_US = datetime.datetime(1970, 1, 1, 0, 0, 0, 1)


@pytest.mark.parametrize(
    ("table", "pred", "kept"),
    [
        # timestamps compare at the finer unit (main: pyarrow)
        (_table(ts=TS_NS), ur.col("ts") == ONE_US, []),
        (_table(ts=TS_NS), ur.col("ts") > ONE_US, [0]),
        (
            _table(ts=pa.array([1, 2], pa.timestamp("s"))),
            ur.col("ts") > np.datetime64(1500000, "us"),
            [1],
        ),
        (
            _table(ts=pa.array([1, 2], pa.timestamp("ms"))),
            ur.col("ts") >= np.datetime64(2, "ms"),
            [1],
        ),
        (
            _table(du=pa.array([1, 2], pa.duration("s"))),
            ur.col("du") > np.timedelta64(1500, "ms"),
            [1],
        ),
        # NaN canonical in total order: above every number, equal to itself,
        # whatever its sign bit; NaN rows survive Parquet statistics.
        (_table(f=[0.5, math.nan, 3.0, math.nan]), ur.col("f") > 1, [1, 2, 3]),
        (_table(r=[NEG_NAN, 2.0, 1.0]), ur.col("r") < 0, []),
        (_table(r=[NEG_NAN, 2.0, 1.0]), ur.col("r") > 1.5, [0, 1]),
        (_table(r=[NEG_NAN, 1.0, math.nan, 2.0]), ur.col("r") == math.nan, [0, 2]),
        # -0.0 == 0.0
        (_table(x=[-0.0, 1.0]), ur.col("x") == 0, [0]),
        (_table(x=[-0.0, 1.0]), ur.col("x") == -0.0, [0]),
        (_table(x=[0.0, 1.0]), ur.col("x") < 0, []),
        # uint64 past 2^63, exact
        (
            _table(u=pa.array([2**63 + 1, 2**63 + 2], pa.uint64())),
            ur.col("u") == np.uint64(2**63 + 1),
            [0],
        ),
        (_table(u=pa.array([2**63 + 1, 2**63 + 2], pa.uint64())), ur.col("u") > 2**63 + 1, [1]),
        # literals pyarrow builds: bytes, wide decimals, a day-unit datetime64
        (_table(b=pa.array([b"a", b"b"])), ur.col("b") == b"a", [0]),
        (_table(s=["a", "b"]), ur.col("s") == b"a", [0]),
        (
            _table(
                x=pa.array(
                    [decimal.Decimal("1" * 39 + ".5"), decimal.Decimal(1)], pa.decimal256(42, 1)
                )
            ),
            ur.col("x") > decimal.Decimal("1" * 39),
            [0],
        ),
        (
            _table(
                x=pa.array(
                    [decimal.Decimal("12345678901234567890123456789.5"), decimal.Decimal(1)],
                    pa.decimal128(31, 1),
                )
            ),
            ur.col("x") >= decimal.Decimal("12345678901234567890123456789.5"),
            [0],
        ),
        (
            _table(
                ts=pa.array(
                    [datetime.datetime(2020, 1, 2), datetime.datetime(2020, 1, 4)],
                    pa.timestamp("us"),
                )
            ),
            ur.col("ts") > np.datetime64("2020-01-03"),
            [1],
        ),
        (
            _table(du=pa.array([3600, 2 * 86400], pa.duration("s"))),
            ur.col("du") > np.timedelta64(1, "D"),
            [1],
        ),
    ],
)
def test_comparisons_keep_the_same_rows_on_every_path(table, pred, kept, tmp_path):
    assert _every_path(table, pred, tmp_path) == kept


@pytest.mark.parametrize(
    ("table", "pred", "message"),
    [
        (
            _table(i=[1, 2]),
            ur.col("i") == "1",
            r"'equal' has no kernel matching input types \(int64, string\)",
        ),
        (_table(f=[1.0, 2.0]), ur.col("f") > "1", r"'greater' has no kernel .* \(double, string\)"),
        (
            _table(ts=pa.array([1, 2], pa.timestamp("us"))),
            ur.col("ts") > "2024-01-01",
            r"'greater' has no kernel .* \(timestamp\[us\], string\)",
        ),
        (
            _table(dt=pa.array([datetime.date(2020, 1, 1)])),
            ur.col("dt") < "2020-01-02",
            r"'less' has no kernel .* \(date32\[day\], string\)",
        ),
        (_table(i=[1, 2]), ur.col("i") == True, r"no kernel .* \(int64, bool\)"),  # noqa: E712
        (_table(i=[1, 2]), ur.col("i") == [1, 2], r"no kernel .* \(int64, list<item: int64>\)"),
        (
            _table(du=pa.array([1], pa.duration("s"))),
            ur.col("du") > 1,
            r"no kernel .* \(duration\[s\], int64\)",
        ),
        # lossy comparisons main refused: an int past 2^53 against a float, a
        # uint64 past 2^63 against a signed int
        (
            _table(i=[2**53 + 1, 2**53]),
            ur.col("i") == float(2**53),
            r"Integer value 9007199254740993 not in range: -9007199254740992 to 9007199254740992",
        ),
        (
            _table(u=pa.array([2**63 + 1, 5], pa.uint64())),
            ur.col("u") == 5,
            r"Integer value 9223372036854775809 not in range: 0 to 9223372036854775807",
        ),
        # aware vs naive at different units: no kernel (the same unit is a ValueError)
        (
            _table(ts=pa.array([1, 2], pa.timestamp("ns", "Europe/Paris"))),
            ur.col("ts") > datetime.datetime(2020, 1, 1),
            r"no kernel .* \(timestamp\[ns, tz=Europe/Paris\], timestamp\[us\]\)",
        ),
        (_table(i=[1, 2]), (ur.col("i") / 0) > 1, r"Divide by zero"),
    ],
)
def test_comparisons_main_rejected_are_compute_errors_on_every_path(table, pred, message, tmp_path):
    with pytest.raises(ur.ComputeError, match=message):
        _every_path(table, pred, tmp_path)
        _rows(ur.from_arrow(table, id="id").filter(pred))


def test_month_and_year_datetime64_are_rejected_clearly():
    nodes = ur.from_arrow(_table(ts=pa.array([1, 2], pa.timestamp("us"))), id="id")
    for unit in ("M", "Y"):
        with pytest.raises(TypeError, match=f"unit '{unit}' has no fixed length"):
            nodes.filter(ur.col("ts") > np.datetime64("2020", unit)).collect()


def test_string_view_compares_with_string():
    # Accepted (pyarrow had no kernel for it; the engine compares the values).
    t = pa.table({"id": [0, 1], "s": pa.array(["a", "b"], pa.string_view())})
    nodes = ur.from_arrow(t, id="id")
    assert [r["id"] for r in _rows(nodes.filter(ur.col("s") == "b"))] == [1]


def test_float_filters_stay_out_of_parquet_pruning(tmp_path, scan_log):
    # min/max statistics leave NaN out, so a float comparison runs after the read.
    path = tmp_path / "f.parquet"
    pq.write_table(_table(f=[0.5, math.nan, 3.0, math.nan]), path, row_group_size=2)
    scan = ur.scan_edges(str(path), src="id", dst="d")
    assert [r["id"] for r in _rows(scan.filter(ur.col("f") > 1))] == [1, 2, 3]
    assert [pushed for _, _, pushed in scan_log()] == [0]
    scan.filter(ur.col("id") > 1).collect()  # an integer comparison is pushed
    assert [pushed for _, _, pushed in scan_log()] == [1]


def test_a_traversal_view_does_not_rerun_a_derived_seed_frame():
    edges = _edges()
    seeds = ur.from_arrow(pa.table({"id": [0, 1, 2]}), id="id").filter(ur.col("id") < 2)
    view = ur.hop(edges, n=1).from_(seeds)
    first = ur.degree(view).collect().to_arrow()
    cached = seeds._seeds_cell.value
    assert cached is not None
    assert ur.degree(view).collect().to_arrow().equals(first)
    assert seeds._seeds_cell.value is cached
    assert edges._index_build_cell.value._mask_build_count() == 1
