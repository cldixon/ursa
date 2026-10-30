"""A scan of a list of paths (#216): the paths, files or globs, read as one
table in the order given. Every result must equal the files read into memory,
concatenated in that order, and run as an in-memory frame: plain collects,
filters, unweighted and weighted graph ops (one index over the union of the
files, with the weights read from the same rows), and the reshaped-topology
cache, keyed by every file the scan reads.
"""

from __future__ import annotations

import os

import pyarrow as pa
import pyarrow.csv as pacsv
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa._execute import _native

pytestmark = pytest.mark.skipif(not ur._NATIVE_AVAILABLE, reason="native extension not built")

PARTS = [
    {"s": [1, 2, 3], "d": [2, 3, 1], "w": [0.5, 1.5, 2.5]},
    {"s": [4, 1], "d": [1, 4], "w": [3.0, 0.25]},
    {"s": [5, 5, 2], "d": [2, 4, 5], "w": [1.0, 2.0, 0.75]},
]


@pytest.fixture
def files(tmp_path):
    """Three Parquet parts, two of them in a subdirectory (for globs), and the
    same parts as CSV."""
    (tmp_path / "more").mkdir()
    names = ["a.parquet", "more/b.parquet", "more/c.parquet"]
    for name, part in zip(names, PARTS, strict=True):
        pq.write_table(pa.table(part), tmp_path / name)
        pacsv.write_csv(pa.table(part), tmp_path / name.replace(".parquet", ".csv"))
    return tmp_path


def _memory(paths):
    """The files read into memory and concatenated in path order."""
    tables = []
    for path in paths:
        if path.endswith(".csv"):
            tables.append(pacsv.read_csv(path))
        else:
            tables.append(pq.read_table(path))
    return ur.EdgeFrame(pa.concat_tables(tables), src="s", dst="d")


def _same(got, want):
    got, want = got.collect().to_arrow(), want.collect().to_arrow()
    assert got.equals(want), (got.to_pydict(), want.to_pydict())


OPS = {
    "collect": lambda e: e,
    "filter": lambda e: e.filter(ur.col("w") > 0.6),
    "sort": lambda e: e.sort("w"),
    "degree": lambda e: ur.degree(e),
    "components": lambda e: e.nodes().with_columns(c=ur.connected_components(e)),
    "pagerank": lambda e: ur.pagerank(e),
    "pagerank_weighted": lambda e: ur.pagerank(e, weight=ur.col("w")),
    "weighted_expression": lambda e: ur.pagerank(e, weight=ur.col("w") * 2 + 1),
    "weighted_view": lambda e: ur.pagerank(e.filter(ur.col("w") > 0.6), weight=ur.col("w")),
    "weighted_reversed": lambda e: ur.pagerank(e.reverse(), weight=ur.col("w")),
    "closeness_weighted": lambda e: e.nodes().with_columns(c=ur.closeness(e, weight=ur.col("w"))),
    "shortest_path_weighted": lambda e: ur.shortest_path(e, 4, 3, weight=ur.col("w")),
    "reshaped_weighted": lambda e: ur.pagerank(e.distinct(), weight=ur.col("w")),
}

ORDERS = {
    "files": ["a.parquet", "more/b.parquet", "more/c.parquet"],
    "files_reordered": ["more/c.parquet", "a.parquet", "more/b.parquet"],
    "glob_then_file": ["more/*.parquet", "a.parquet"],
    "csv": ["more/b.csv", "a.csv"],
    "mixed_formats": ["a.csv", "more/b.parquet"],
}


@pytest.mark.parametrize("order", sorted(ORDERS))
@pytest.mark.parametrize("op", sorted(OPS))
def test_a_list_of_paths_reads_as_the_files_concatenated(files, order, op):
    names = ORDERS[order]
    paths = [str(files / n) for n in names]
    expanded = []
    for n in names:
        if "*" in n:
            expanded += sorted(str(p) for p in files.glob(n))
        else:
            expanded.append(str(files / n))
    _same(OPS[op](ur.scan_edges(paths, src="s", dst="d")), OPS[op](_memory(expanded)))


def test_one_path_in_a_list_is_that_path(files):
    path = str(files / "a.parquet")
    _same(ur.scan_edges([path], src="s", dst="d"), ur.scan_edges(path, src="s", dst="d"))
    weighted = lambda e: ur.pagerank(e, weight=ur.col("w"))  # noqa: E731
    _same(
        weighted(ur.scan_edges([path], src="s", dst="d")),
        weighted(ur.scan_edges(path, src="s", dst="d")),
    )


def test_the_weighted_and_unweighted_ops_share_one_index(files):
    paths = [str(files / "more/c.parquet"), str(files / "a.parquet")]
    e = ur.scan_edges(paths, src="s", dst="d")
    before = _native()._topology_build_count()
    weighted = ur.pagerank(e, weight=ur.col("w")).collect().to_arrow()
    plain = ur.degree(e).collect().to_arrow()
    assert _native()._topology_build_count() - before == 1
    mem = _memory(paths)
    assert weighted.equals(ur.pagerank(mem, weight=ur.col("w")).collect().to_arrow())
    assert plain.equals(ur.degree(mem).collect().to_arrow())


def test_a_node_scan_reads_a_list_of_paths(files):
    nodes = [{"id": [1, 2], "x": [10, 20]}, {"id": [3, 4, 5], "x": [30, 40, 50]}]
    paths = []
    for i, part in enumerate(nodes):
        path = files / f"nodes{i}.parquet"
        pq.write_table(pa.table(part), path)
        paths.append(str(path))
    got = ur.read_nodes(paths[::-1], id="id").to_arrow()
    want = pa.concat_tables([pq.read_table(p) for p in paths[::-1]])
    assert got.equals(want)
    edges = ur.scan_edges(
        [str(files / "a.parquet"), str(files / "more/c.parquet")], src="s", dst="d"
    )
    joined = ur.scan_nodes(paths, id="id").with_columns(p=ur.pagerank(edges, weight=ur.col("w")))
    mem = ur.NodeFrame(pa.concat_tables([pq.read_table(p) for p in paths]), id="id")
    mem_edges = _memory([str(files / "a.parquet"), str(files / "more/c.parquet")])
    _same(joined, mem.with_columns(p=ur.pagerank(mem_edges, weight=ur.col("w"))))


def test_on_null_drop_counts_the_rows_of_every_path(files):
    pq.write_table(pa.table({"s": [1, None], "d": [2, 3], "w": [1.0, 2.0]}), files / "n1.parquet")
    pq.write_table(pa.table({"s": [3, 4], "d": [None, 1], "w": [3.0, 4.0]}), files / "n2.parquet")
    paths = [str(files / "n1.parquet"), str(files / "n2.parquet")]
    e = ur.scan_edges(paths, src="s", dst="d", on_null="drop")
    with pytest.warns(UserWarning, match="dropped 2"):
        got = ur.pagerank(e, weight=ur.col("w")).collect().to_arrow()
    with pytest.warns(UserWarning, match="dropped 2"):
        mem = ur.EdgeFrame(
            pa.concat_tables([pq.read_table(p) for p in paths]), src="s", dst="d", on_null="drop"
        )
    want = ur.pagerank(mem, weight=ur.col("w")).collect().to_arrow()
    assert got.equals(want)


def test_paths_with_different_columns_raise_naming_each_difference(files):
    pq.write_table(pa.table({"s": [1], "d": [2]}), files / "narrow.parquet")
    pq.write_table(pa.table({"s": [1], "d": [2], "w": [1]}), files / "int_w.parquet")
    pq.write_table(
        pa.table({"s": [1], "d": [2], "w": pa.array([1.0], pa.float64())}).cast(
            pa.schema([("s", pa.int64()), ("d", pa.int64()), ("w", pa.float64(), False)])
        ),
        files / "required_w.parquet",
    )
    pq.write_table(
        pa.table({"s": ["a"], "d": ["b"], "w": [1.0]}).cast(
            pa.schema([("s", pa.large_string()), ("d", pa.string()), ("w", pa.float64())])
        ),
        files / "large.parquet",
    )
    a = str(files / "a.parquet")
    cases = {
        "narrow.parquet": r"'[^']*a.parquet' has the columns \[s, d, w\] and "
        r"'[^']*narrow.parquet' has \[s, d\]. Give every file the same columns",
        "int_w.parquet": r"Column 'w' is double in '[^']*a.parquet' and int64 in "
        r"'[^']*int_w.parquet'. Write the files with matching types",
    }
    for other, why in cases.items():
        e = ur.scan_edges([a, str(files / other)], src="s", dst="d")
        for run in (e.collect, lambda e=e: ur.pagerank(e, weight=ur.col("w")).collect()):
            with pytest.raises(
                ur.ComputeError,
                match=rf"^scan_edges\(\['[^']*a.parquet', '[^']*{other}'\]\): the paths in a "
                r"list are read as one table, so each needs the same columns, with the same "
                r"types, in the same order. " + why,
            ):
                run()
    # Nullability may differ: the column is nullable in the union.
    both = ur.scan_edges([a, str(files / "required_w.parquet")], src="s", dst="d")
    assert both.collect().to_arrow().num_rows == 4
    # string and large_string are different types, and named so.
    strings = files / "strings.parquet"
    pq.write_table(pa.table({"s": ["c"], "d": ["d"], "w": [2.0]}), strings)
    e = ur.scan_edges([str(strings), str(files / "large.parquet")], src="s", dst="d")
    with pytest.raises(ur.ComputeError, match=r"Column 's' is string in .* and large_string in"):
        e.collect()


def test_csv_types_are_inferred_per_file(files):
    # Each CSV path infers its own types: a column of ints in one file and of
    # floats in the other is a type mismatch, named with what to do.
    (files / "ints.csv").write_text("s,d,w\n1,2,1\n")
    (files / "floats.csv").write_text("s,d,w\n2,3,0.5\n")
    e = ur.scan_edges([str(files / "ints.csv"), str(files / "floats.csv")], src="s", dst="d")
    with pytest.raises(ur.ComputeError, match=r"Column 'w' is int64 in .* write 1.0 for a float"):
        e.collect()


def test_tuples_and_pathlib_paths(files):
    a, c = files / "a.parquet", files / "more/c.parquet"
    want = ur.scan_edges([str(a), str(c)], src="s", dst="d").collect().to_arrow()
    for path in ((str(a), str(c)), [a, c], (a, str(c))):
        assert ur.read_edges(path, src="s", dst="d").to_arrow().equals(want)
    one = ur.read_edges(a, src="s", dst="d").to_arrow()
    assert one.equals(ur.read_edges(str(a), src="s", dst="d").to_arrow())
    assert ur.read_nodes((a,), id="s").to_arrow().equals(pq.read_table(a))


def test_paths_are_checked_where_the_scan_is_made(files):
    # The error collect() raised for a path it cannot read, raised at the call.
    msg = r"^scan_edges\('b.txt'\): only \.parquet and \.csv files can be scanned"
    with pytest.raises(ur.ComputeError, match=msg):
        ur.read_edges((str(files / "a.parquet"), "b.txt"), src="s", dst="d")
    with pytest.raises(ur.ComputeError, match="query string"):
        ur.scan_edges("https://host/x.csv?token=1", src="s", dst="d")
    with pytest.raises(TypeError, match=r"read_nodes\(\): a path must be a string or a pathlib"):
        ur.read_nodes([str(files / "a.parquet"), 3], id="id")  # ty: ignore[invalid-argument-type]
    with pytest.raises(TypeError, match="got int"):
        ur.scan_edges(3, src="s", dst="d")  # ty: ignore[invalid-argument-type]


def test_an_empty_list_raises_where_the_scan_is_made():
    with pytest.raises(ValueError, match=r"scan_edges\(\): the list of paths is empty"):
        ur.scan_edges([], src="s", dst="d")
    with pytest.raises(ValueError, match=r"scan_nodes\(\): the list of paths is empty"):
        ur.scan_nodes([], id="id")
    with pytest.raises(ValueError, match=r"read_edges\(\): the list of paths is empty"):
        ur.read_edges((), src="s", dst="d")


def test_a_missing_path_in_a_list_raises(files):
    e = ur.scan_edges([str(files / "a.parquet"), str(files / "nope.parquet")], src="s", dst="d")
    with pytest.raises(Exception, match=r"nope\.parquet"):
        e.collect()


def test_the_reshaped_topology_cache_keys_every_file(files):
    # The cache key covers every path's files (#206): reading the same list
    # again reuses the index, and rewriting any one file rebuilds it.
    a, c = files / "a.parquet", files / "more/c.parquet"
    paths = [str(a), str(c)]
    e = ur.scan_edges(paths, src="s", dst="d")
    ur.degree(e.distinct()).collect()
    before = _native()._topology_build_count()
    again = ur.degree(ur.scan_edges(paths, src="s", dst="d").distinct()).collect().to_arrow()
    assert _native()._topology_build_count() == before
    # Rewrite the second file only, with a later modification time.
    pq.write_table(pa.table({"s": [7, 8], "d": [8, 7], "w": [1.0, 1.0]}), c)
    stat = os.stat(c)
    os.utime(c, ns=(stat.st_atime_ns, stat.st_mtime_ns + 10**9))
    fresh = ur.degree(ur.scan_edges(paths, src="s", dst="d").distinct()).collect().to_arrow()
    assert _native()._topology_build_count() == before + 1
    assert fresh.column("id").to_pylist() == [1, 2, 3, 7, 8]
    assert again.column("id").to_pylist() == [1, 2, 3, 5, 4]
    # The order of the paths is part of the key.
    swapped = ur.degree(ur.scan_edges(paths[::-1], src="s", dst="d").distinct())
    assert swapped.collect().to_arrow().column("id").to_pylist() == [7, 8, 1, 2, 3]


def test_explain_and_schema_of_a_list_scan(files):
    e = ur.scan_edges([str(files / "a.parquet"), str(files / "more/*.parquet")], src="s", dst="d")
    assert e.schema() == e.collect().to_arrow().schema
    text = e.explain()
    assert "a.parquet" in text and "more/*.parquet" in text
