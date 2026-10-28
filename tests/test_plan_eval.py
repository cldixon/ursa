"""One expression evaluator (#192): every filter, subgraph mask and plain-source
read runs in Rust.

These pin what moved: traversals honour a filtered edge frame (the one behaviour
fix), the NaN semantics each path keeps, typed literals, the error type a kernel
type mismatch raises, scan pushdown, and the Rust-side caches.
"""

import datetime
import decimal
import math

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
    with pytest.raises(ur.ComputeError, match="Cannot cast string 'x'"):
        nodes.filter(ur.col("i") == "x").collect()
    with pytest.raises(ValueError, match="filter references unknown column 'nope'"):
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
    frame = {
        "plain": lambda: nodes.filter(pred),
        "mask": lambda: ur.degree(edges.filter(pred))._frame(),
        "engine": lambda: nodes.with_columns(dg=ur.degree(edges)).filter(pred),
    }[path]()
    with pytest.raises(ValueError, match=message):
        frame.collect()
    # The same instant on both sides compares.
    same = ur.col("utc") > AWARE
    assert [r["id"] for r in _rows(nodes.filter(same).select("id"))] == [2]
