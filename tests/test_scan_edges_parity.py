"""#160: a scanned edge frame behaves exactly like the same table in memory.

`scan_edges(path)` and `EdgeFrame(<that file read into pyarrow>)` must give the same
column set, order, types and row order on every relational path: a plain collect,
its filter/sort/select/head/distinct/sample/rename/group_by/join tail, and the
sinks. The graph-op paths keep their narrow scans (endpoints, plus weight columns
for a weighted op), and a weight expression may reference the endpoints (#154).
"""

import pyarrow as pa
import pyarrow.csv as pcsv
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa import _execute

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `uv sync`)"
)

INT_TABLE = pa.table(
    {
        "s": [1, 2, 3, 1, 2],
        "d": [2, 3, 1, 3, 3],
        "w": [1.0, 2.0, 3.0, 4.0, 2.0],
        "label": ["a", "b", "c", "a", "b"],
    }
)
STR_TABLE = pa.table(
    {
        "s": ["x", "y", "z", "x", "y"],
        "d": ["y", "z", "x", "z", "z"],
        "w": [1.0, 2.0, 3.0, 4.0, 2.0],
        "label": ["a", "b", "c", "a", "b"],
    }
)


def _recast(table, fn):
    return pa.table(
        {
            c: fn(table[c]) if pa.types.is_string(table[c].type) else table[c]
            for c in table.column_names
        }
    )


# (name, table, format). Each file is read back with pyarrow for the in-memory twin,
# so both frames see exactly the types the file stores.
SOURCES = [
    ("parquet-int64", INT_TABLE, "parquet"),
    ("csv-int", INT_TABLE, "csv"),
    (
        "parquet-int32",
        INT_TABLE.set_column(0, "s", INT_TABLE["s"].cast(pa.int32())).set_column(
            1, "d", INT_TABLE["d"].cast(pa.int32())
        ),
        "parquet",
    ),
    ("parquet-string", STR_TABLE, "parquet"),
    ("csv-string", STR_TABLE, "csv"),
    ("parquet-string-view", _recast(STR_TABLE, lambda c: c.cast(pa.string_view())), "parquet"),
    ("parquet-dictionary", _recast(STR_TABLE, lambda c: c.dictionary_encode()), "parquet"),
]


@pytest.fixture(params=SOURCES, ids=[s[0] for s in SOURCES])
def pair(request, tmp_path):
    """(scanned frame, in-memory frame, first src id) over the same file."""
    _, table, fmt = request.param
    path = tmp_path / f"edges.{fmt}"
    if fmt == "parquet":
        pq.write_table(table, str(path))
        mem_table = pq.read_table(str(path))
    else:
        pcsv.write_csv(table, str(path))
        mem_table = pcsv.read_csv(str(path))
    scan = ur.scan_edges(str(path), src="s", dst="d")
    mem = ur.EdgeFrame(mem_table, src="s", dst="d")
    return scan, mem, mem_table.column("s")[0].as_py()


def _outcome(build):
    """A frame's collected output (names, types, rows), or the error it raises — so
    a path neither frame supports yet (a sort on a dictionary column, #159) still
    has to fail the same way on both."""
    try:
        t = build().collect().to_arrow()
    except Exception as exc:  # the error itself is what is compared
        return (type(exc).__name__, str(exc))
    return (t.column_names, [str(f.type) for f in t.schema], t.to_pylist())


RELATIONAL = {
    "collect": lambda e: e,
    "filter-attr": lambda e: e.filter(ur.col("label") == "a"),
    "filter-numeric": lambda e: e.filter(ur.col("w") > 1.5),
    "sort-attr": lambda e: e.sort("w", descending=True),
    "sort-string-attr": lambda e: e.sort("label"),
    "select-attr": lambda e: e.select("label"),
    "select-mixed": lambda e: e.select("w", "s"),
    "select-after-filter-sort": lambda e: e.filter(ur.col("w") > 1).sort("w").select("label"),
    "select-renamed": lambda e: e.rename({"label": "tag"}).select("tag", "d"),
    "head": lambda e: e.head(2),
    "distinct": lambda e: e.distinct().sort("w"),
    "distinct-select": lambda e: e.distinct().select("label").sort("label"),
    "sample": lambda e: e.sample(3, seed=7),
    "rename": lambda e: e.rename({"label": "tag", "w": "weight"}),
    "reverse": lambda e: e.reverse(),
    "group-by": lambda e: e.group_by("label").agg(total=ur.col("w").sum(), n=ur.col("w").count()),
    "group-by-sorted": lambda e: e.group_by("label").agg(total=ur.col("w").sum()).sort("total"),
}


@pytest.mark.parametrize("op", list(RELATIONAL))
def test_relational_paths_match_in_memory(pair, op):
    scan, mem, _ = pair
    fn = RELATIONAL[op]
    assert _outcome(lambda: fn(scan)) == _outcome(lambda: fn(mem))


def test_filter_on_an_endpoint_role_matches_in_memory(pair):
    scan, mem, first = pair
    for e_scan, e_mem in [(scan, mem), (scan.reverse(), mem.reverse())]:
        got = _outcome(lambda e=e_scan: e.filter(ur.src() == first))
        want = _outcome(lambda e=e_mem: e.filter(ur.src() == first))
        assert got == want


def test_plain_collect_returns_every_column(pair):
    scan, _, _ = pair
    assert scan.collect().columns == ["s", "d", "w", "label"]


def test_join_matches_in_memory(pair):
    scan, mem, _ = pair
    rows = mem.collect().to_dicts()
    ids = sorted({r["s"] for r in rows} | {r["d"] for r in rows})
    nodes = ur.from_arrow(pa.table({"s": ids, "rank": list(range(len(ids)))}), id="s")
    for build in (
        lambda e: e.join(nodes, on="s").sort("w"),
        lambda e: e.join(nodes, on="s", how="left").filter(ur.col("label") != "c").sort("w"),
        lambda e: e.join(nodes, on="s").group_by("label").agg(r=ur.col("rank").sum()),
    ):
        assert _outcome(lambda b=build: b(scan)) == _outcome(lambda b=build: b(mem))


@pytest.mark.parametrize("sink", ["parquet", "csv"])
def test_sinks_match_in_memory(pair, tmp_path, sink):
    scan, mem, _ = pair
    a, b = tmp_path / f"scan.{sink}", tmp_path / f"mem.{sink}"
    getattr(scan, f"sink_{sink}")(str(a))
    getattr(mem, f"sink_{sink}")(str(b))
    read = pq.read_table if sink == "parquet" else pcsv.read_csv
    assert read(str(a)).equals(read(str(b)))


def test_graph_ops_match_in_memory(pair):
    scan, mem, first = pair

    def plans(e):
        sub = e.filter(ur.col("label") != "c")  # a subgraph view over an attribute
        hop = ur.hop(e, 1).from_([first])
        return [
            e.nodes().with_columns(x=ur.degree(e)),
            e.nodes().with_columns(x=ur.pagerank(e, weight=ur.col("w"))),
            sub.nodes().with_columns(x=ur.degree(sub)),
            hop.nodes().with_columns(x=ur.degree(hop)),
        ]

    for p_scan, p_mem in zip(plans(scan), plans(mem), strict=True):
        assert _outcome(lambda p=p_scan: p.sort("id")) == _outcome(lambda p=p_mem: p.sort("id"))


def test_weight_expression_may_reference_the_endpoints(tmp_path):
    # #154: weight= over the src/dst column of a scanned frame.
    path = tmp_path / "edges.parquet"
    pq.write_table(INT_TABLE, str(path))
    scan = ur.scan_edges(str(path), src="s", dst="d")
    mem = ur.EdgeFrame(pq.read_table(str(path)), src="s", dst="d")
    for weight in (ur.col("s") * 1.0, ur.col("d") + ur.col("w")):
        got = scan.nodes().with_columns(x=ur.pagerank(scan, weight=weight)).sort("id")
        want = mem.nodes().with_columns(x=ur.pagerank(mem, weight=weight)).sort("id")
        assert got.collect().to_dicts() == want.collect().to_dicts()
    got = ur.shortest_path(scan, 1, 3, weight=ur.col("d") * 1.0).collect().to_dicts()
    want = ur.shortest_path(mem, 1, 3, weight=ur.col("d") * 1.0).collect().to_dicts()
    assert got == want


def test_on_null_drop_applies_to_the_full_table(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("s,label,d\n1,a,2\n,b,3\n2,c,\n3,d,1\n")
    scan = ur.scan_edges(str(path), src="s", dst="d", on_null="drop")
    with pytest.warns(UserWarning, match="dropped 2"):
        out = scan.collect()
    assert out.columns == ["s", "label", "d"]
    assert out.to_dicts() == [{"s": 1, "label": "a", "d": 2}, {"s": 3, "label": "d", "d": 1}]


def test_missing_endpoint_column_is_a_column_error(tmp_path):
    path = tmp_path / "edges.csv"
    path.write_text("s,d\n1,2\n")
    with pytest.raises(ur.ColumnNotFoundError, match="nope"):
        ur.scan_edges(str(path), src="s", dst="nope").collect()


class _ScanCalls:
    """The native scans since the last ``clear()``, as ``(kind, projected columns)``
    pairs, read from the native scan log (the scans run in Rust since #192)."""

    def __init__(self) -> None:
        self._calls: list[tuple[str, list[str]]] = []
        self.pushed: list[int] = []
        _execute._native()._scan_log(True)

    def _pull(self) -> list[tuple[str, list[str]]]:
        for kind, columns, pushed in _execute._native()._scan_log(True):
            self._calls.append((kind, columns))
            self.pushed.append(pushed)
        return self._calls

    def clear(self) -> None:
        self._pull()
        self._calls.clear()
        self.pushed.clear()

    def __eq__(self, other: object) -> bool:
        return self._pull() == other

    def __contains__(self, item: object) -> bool:
        return item in self._pull()

    def __iter__(self):
        return iter(list(self._pull()))


@pytest.fixture
def scan_calls():
    """Record every native edge scan: (kind, projected columns)."""
    calls = _ScanCalls()
    yield calls
    _execute._native()._scan_log(False)


def _wide(tmp_path):
    path = tmp_path / "wide.parquet"
    wide = INT_TABLE.append_column("blob", pa.array(["x" * 50] * INT_TABLE.num_rows))
    pq.write_table(wide, str(path))
    return ur.scan_edges(str(path), src="s", dst="d")


def test_select_pushes_its_projection_into_the_scan(tmp_path, scan_calls):
    edges = _wide(tmp_path)
    out = edges.filter(ur.col("w") > 1).select("label").collect()
    assert out.columns == ["label"]
    assert scan_calls == [("table", ["d", "label", "s", "w"])]
    scan_calls.clear()
    # A distinct runs over every column before the select, so it reads them all.
    edges.distinct().select("label").collect()
    assert scan_calls == [("table", [])]


def test_graph_ops_keep_their_narrow_scans(tmp_path, scan_calls):
    edges = _wide(tmp_path)
    ur.degree(edges).collect()
    assert scan_calls == [("index", [])]  # streamed src/dst build (#149)
    scan_calls.clear()
    ur.pagerank(edges, weight=ur.col("w")).collect()
    assert scan_calls == [("edges", ["w"])]
    scan_calls.clear()
    # A subgraph view's mask reads the endpoints and the predicate's columns only.
    sub = edges.filter(ur.col("label") == "a")
    ur.degree(sub).collect()
    assert ("table", ["d", "label", "s"]) in scan_calls
    assert all(kind != "table" or cols for kind, cols in scan_calls)
