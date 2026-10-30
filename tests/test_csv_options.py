"""CSV options on scans (#216): ``delimiter``, ``has_header``, ``quote``,
``escape``, ``null_value`` and ``comment`` on ``scan_edges`` / ``scan_nodes`` /
``read_edges`` / ``read_nodes``.

Each layout is read by Ursa and by ``pyarrow.csv`` with the same options, and
the tables must be equal. pyarrow has no comment option, so its input has the
comment lines removed first. An empty field reads as null in both (Ursa's rule
with or without ``null_value``).
"""

from __future__ import annotations

import io
from typing import Any

import pyarrow as pa
import pyarrow.csv as pacsv
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(not ur._NATIVE_AVAILABLE, reason="native extension not built")


def _pyarrow(text: str, **opts) -> pa.Table:
    """``text`` read by pyarrow with the options a scan was given."""
    comment = opts.get("comment")
    if comment is not None:
        text = "".join(
            line for line in text.splitlines(keepends=True) if not line.startswith(comment)
        )
    parse = pacsv.ParseOptions(
        delimiter=opts.get("delimiter", ","),
        quote_char=opts.get("quote", '"'),
        escape_char=opts.get("escape") or False,
    )
    nulls = [""] + ([opts["null_value"]] if opts.get("null_value") is not None else [])
    convert = pacsv.ConvertOptions(null_values=nulls, strings_can_be_null=True)
    read = pacsv.ReadOptions(autogenerate_column_names=not opts.get("has_header", True))
    table = pacsv.read_csv(
        io.BytesIO(text.encode()),
        parse_options=parse,
        convert_options=convert,
        read_options=read,
    )
    if not opts.get("has_header", True):
        table = table.rename_columns([f"column_{i + 1}" for i in range(table.num_columns)])
    return table


# (file text, scan options, src, dst)
LAYOUTS = {
    "semicolon": ("s;d;w;label\n1;2;0.5;a\n2;3;1.5;b\n3;1;2.5;c\n", {"delimiter": ";"}, "s", "d"),
    "tab": ("s\td\tw\n1\t2\t0.5\n2\t3\t\n3\t1\t2.5\n", {"delimiter": "\t"}, "s", "d"),
    "pipe_no_header": (
        "1|2|0.5\n2|3|1.5\n3|1|2.5\n",
        {"delimiter": "|", "has_header": False},
        "column_1",
        "column_2",
    ),
    "single_quote": (
        "s,d,label\n1,2,'a,b'\n2,3,'c'\n3,1,'d''e'\n",
        {"quote": "'"},
        "s",
        "d",
    ),
    "escape": (
        's,d,label\n1,2,"say \\"hi\\""\n2,3,"x\\,y"\n3,1,plain\n',
        {"escape": "\\"},
        "s",
        "d",
    ),
    "null_value": (
        "s,d,w,n,label\n1,2,NA,7,NA\n2,3,1.5,NA,b\n3,1,2.5,9,\n",
        {"null_value": "NA"},
        "s",
        "d",
    ),
    "comment": (
        "# written by hand\ns,d,w\n1,2,0.5\n# a note\n2,3,1.5\n3,1,2.5\n",
        {"comment": "#"},
        "s",
        "d",
    ),
    "all_together": (
        "% export\ns;d;w;label\n1;2;-;'x;y'\n% skip\n2;3;1.5;'it''s'\n3;1;2.5;-\n",
        {"delimiter": ";", "quote": "'", "null_value": "-", "comment": "%"},
        "s",
        "d",
    ),
}


@pytest.fixture(params=sorted(LAYOUTS))
def layout(request, tmp_path):
    text, opts, src, dst = LAYOUTS[request.param]
    path = tmp_path / f"{request.param}.csv"
    path.write_text(text)
    return str(path), text, opts, src, dst


def test_a_scan_reads_what_pyarrow_reads_with_the_same_options(layout):
    path, text, opts, src, dst = layout
    want = _pyarrow(text, **opts)
    got = ur.read_edges(path, src=src, dst=dst, **opts).to_arrow()
    assert got.equals(want), (got, want)
    nodes = ur.read_nodes(path, id=src, **opts).to_arrow()
    assert nodes.equals(want), (nodes, want)


def test_graph_ops_over_a_csv_with_options_match_the_table_in_memory(layout):
    path, text, opts, src, dst = layout
    scan = ur.scan_edges(path, src=src, dst=dst, **opts)
    mem = ur.EdgeFrame(_pyarrow(text, **opts), src=src, dst=dst)
    for frame in (lambda e: ur.degree(e), lambda e: ur.pagerank(e)):
        assert frame(scan).collect().to_arrow().equals(frame(mem).collect().to_arrow())
    assert scan.schema() == scan.collect().to_arrow().schema


def test_a_filter_reads_the_parsed_values(tmp_path):
    path = tmp_path / "e.csv"
    path.write_text("s;d;w\n1;2;NA\n2;3;1.5\n3;1;2.5\n")
    e = ur.scan_edges(str(path), src="s", dst="d", delimiter=";", null_value="NA")
    got = e.filter(ur.col("w") > 2).collect().to_arrow()
    assert got.to_pydict() == {"s": [3], "d": [1], "w": [2.5]}
    assert e.collect().to_arrow().column("w").to_pylist() == [None, 1.5, 2.5]


def test_a_null_value_in_an_endpoint_follows_on_null(tmp_path):
    path = tmp_path / "e.csv"
    path.write_text("s,d\n1,2\nNA,3\n3,1\n")
    with pytest.raises(Exception, match="null"):
        ur.degree(ur.scan_edges(str(path), src="s", dst="d", null_value="NA")).collect()
    e = ur.scan_edges(str(path), src="s", dst="d", null_value="NA", on_null="drop")
    with pytest.warns(UserWarning, match="dropped 1"):
        got = ur.degree(e).collect().to_arrow()
    assert got.column("id").to_pylist() == [1, 2, 3]


def test_the_defaults_build_the_same_scan_as_no_options(tmp_path):
    path = tmp_path / "e.csv"
    path.write_text("s,d\n1,2\n2,3\n")
    plain = ur.scan_edges(str(path), src="s", dst="d")
    given = ur.scan_edges(
        str(path),
        src="s",
        dst="d",
        delimiter=",",
        has_header=True,
        quote='"',
        escape=None,
        null_value=None,
        comment=None,
    )
    assert given.explain() == plain.explain()
    assert given.collect().to_arrow().equals(plain.collect().to_arrow())


def test_scans_with_different_options_never_share_a_reshaped_topology(tmp_path):
    # The reshaped-topology cache keys a scan by its files and its CSV options.
    path = tmp_path / "e.csv"
    path.write_text("s,d\na,b\nb,c\n#c,a\n")
    plain = ur.scan_edges(str(path), src="s", dst="d")
    skips = ur.scan_edges(str(path), src="s", dst="d", comment="#")
    for _ in range(2):
        got = ur.degree(plain.distinct()).collect().to_arrow()
        assert got.column("id").to_pylist() == ["a", "b", "c", "#c"]
        got = ur.degree(skips.distinct()).collect().to_arrow()
        assert got.column("id").to_pylist() == ["a", "b", "c"]
        assert got.column("degree").to_pylist() == [1, 1, 0]


BAD = [
    (
        {"delimiter": ";;"},
        ValueError,
        r"scan_edges\(\): delimiter must be one ASCII character; got ';;'",
    ),
    ({"delimiter": ""}, ValueError, "delimiter must be one ASCII character"),
    ({"delimiter": "é"}, ValueError, "delimiter must be one ASCII character"),
    ({"delimiter": 9}, ValueError, "delimiter must be one ASCII character; got 9"),
    ({"quote": "\n"}, ValueError, "quote cannot be a line break"),
    ({"delimiter": "\r"}, ValueError, "delimiter cannot be a line break"),
    ({"quote": ","}, ValueError, "delimiter and quote are both ','"),
    ({"escape": '"'}, ValueError, "quote and escape are both"),
    ({"comment": ";", "delimiter": ";"}, ValueError, "delimiter and comment are both"),
    ({"escape": "ab"}, ValueError, "escape must be one ASCII character"),
    ({"has_header": 1}, TypeError, "has_header must be True or False; got 1"),
    ({"has_header": "no"}, TypeError, "has_header must be True or False"),
    ({"null_value": 0}, TypeError, "null_value must be a string or None; got 0"),
]


@pytest.mark.parametrize(("opts", "exc", "match"), BAD)
def test_bad_options_raise_where_the_scan_is_made(opts, exc, match):
    with pytest.raises(exc, match=match):
        ur.scan_edges("e.csv", src="s", dst="d", **opts)
    with pytest.raises(exc, match=match.replace("scan_edges", "scan_nodes")):
        ur.scan_nodes("e.csv", id="s", **opts)


def test_csv_options_on_a_parquet_scan_raise():
    msg = r"scan_edges\(\): the CSV options \(delimiter=, comment=\) only apply to \.csv files"
    with pytest.raises(ValueError, match=msg):
        ur.scan_edges("e.parquet", src="s", dst="d", delimiter=";", comment="#")
    with pytest.raises(ValueError, match=r"only apply to \.csv files"):
        ur.scan_nodes(["a.parquet", "b.parquet"], id="id", has_header=False)
    with pytest.raises(ValueError, match=r"only apply to \.csv files"):
        ur.read_edges("e.parquet", src="s", dst="d", null_value="NA")
    # A list with a CSV path takes them; so does a glob, and the default values.
    ur.scan_edges(["a.parquet", "b.CSV"], src="s", dst="d", delimiter=";")
    ur.scan_edges("dir/*.csv", src="s", dst="d", delimiter=";")
    ur.scan_edges("e.parquet", src="s", dst="d", delimiter=",", null_value=None)


def test_a_null_value_that_is_a_regex_metacharacter_is_read_literally(tmp_path):
    path = tmp_path / "e.csv"
    path.write_text("s,d,w\n1,2,.*\n2,3,1.5\n3,1,x\n")
    got = ur.read_edges(str(path), src="s", dst="d", null_value=".*").to_arrow()
    assert got.column("w").to_pylist() == [None, "1.5", "x"]


def test_without_a_header_the_columns_are_numbered(tmp_path):
    path = tmp_path / "e.csv"
    path.write_text("1,2\n2,3\n")
    e = ur.scan_edges(str(path), src="column_1", dst="column_2", has_header=False)
    assert e.schema().names == ["column_1", "column_2"]
    assert e.collect().to_arrow().num_rows == 2


@pytest.mark.parametrize("w", ["NA", ""], ids=["null_value", "empty"])
def test_a_column_with_no_values_reads_as_null_with_a_null_value(tmp_path, w):
    # DataFusion types a column with no values in its sample `null`; it reads
    # as nulls with `null_value` too, as it does without.
    path = tmp_path / "e.csv"
    path.write_text(f"s,d,w\n1,2,{w}\n2,3,{w}\n")
    got = ur.read_edges(str(path), src="s", dst="d", null_value="NA").to_arrow()
    assert got.schema.field("w").type == pa.null()
    assert got.column("w").to_pylist() == [None, None]
    if not w:
        assert got.equals(ur.read_edges(str(path), src="s", dst="d").to_arrow())


# Values of one column, past DataFusion's 1,000-row inference sample where the
# last value differs, so it is parsed with the type the sample inferred.
SAMPLE = 1000
VALUES = {
    "bool": ["true", "FALSE", "True"],
    "bool_bad_past_sample": ["true"] * SAMPLE + ["yes"],
    "bool_t_past_sample": ["false"] * SAMPLE + ["t"],
    "int": ["1", "-2", "+3", "0"],
    "int_float_past_sample": ["1"] * SAMPLE + ["1.5"],
    "int_overflow_past_sample": ["1"] * SAMPLE + ["9223372036854775808"],
    "float": ["1.5", "-0.0", "1e3", "2"],
    "float_bad_past_sample": ["1.5"] * SAMPLE + ["abc"],
    "timestamp": ["2020-01-01T00:00:00", "2020-01-02 10:30:00"],
    "timestamp_bad_past_sample": ["2020-01-01T00:00:00"] * SAMPLE + ["2020-13-01T00:00:00"],
    "date": ["2020-01-01", "2024-02-29"],
    "date_bad_past_sample": ["2020-01-01"] * SAMPLE + ["2021-02-30"],
    "strings": ["a", "", "NAN", "b"],
}


def _read(path, **opts):
    """The table, or the error's class and message."""
    try:
        return ur.read_edges(str(path), src="s", dst="d", **opts).to_arrow()
    except Exception as e:
        return (type(e).__name__, str(e))


@pytest.mark.parametrize("case", sorted(VALUES))
def test_a_null_value_changes_only_which_fields_are_null(tmp_path, case):
    # With a `null_value`, each type parses (and fails) exactly as without one.
    values = VALUES[case]
    rows = "".join(f"{i},{i + 1},{v}\n" for i, v in enumerate(values))
    plain = tmp_path / "plain.csv"
    plain.write_text("s,d,v\n" + rows)
    assert _read(plain, null_value="NA") == _read(plain)
    # The null value reads as an empty field would: NA in a few rows, against
    # the same file with those fields empty.
    marked = [("NA" if i in (1, len(values) - 2) else v) for i, v in enumerate(values)]
    with_na = tmp_path / "na.csv"
    with_na.write_text("s,d,v\n" + "".join(f"{i},{i + 1},{v}\n" for i, v in enumerate(marked)))
    empty = tmp_path / "empty.csv"
    empty.write_text(
        "s,d,v\n" + "".join(f"{i},{i + 1},{'' if v == 'NA' else v}\n" for i, v in enumerate(marked))
    )
    assert _read(with_na, null_value="NA") == _read(empty)


ALIASES = [
    ("sep", "delimiter"),
    ("separator", "delimiter"),
    ("header", "has_header"),
    ("na_values", "null_value"),
    ("null_values", "null_value"),
    ("quotechar", "quote"),
    ("escapechar", "escape"),
]


@pytest.mark.parametrize(("alias", "option"), ALIASES)
def test_another_librarys_option_name_says_which_to_use(alias, option):
    for fn, kw in (
        (ur.scan_edges, {"src": "s", "dst": "d"}),
        (ur.read_edges, {"src": "s", "dst": "d"}),
        (ur.scan_nodes, {"id": "s"}),
        (ur.read_nodes, {"id": "s"}),
    ):
        msg = (
            rf"^{fn.__name__}\(\) got an unexpected keyword argument '{alias}'; "
            rf"use {option}= instead\.$"
        )
        with pytest.raises(TypeError, match=msg):
            opts: dict[str, Any] = {**kw, alias: ";"}
            fn("e.csv", **opts)


def test_an_unknown_option_names_the_function_called():
    with pytest.raises(
        TypeError, match=r"^read_edges\(\) got an unexpected keyword argument 'foo'\.$"
    ):
        ur.read_edges("e.csv", src="s", dst="d", foo=1)


def test_escape_applies_inside_a_quoted_field(tmp_path):
    path = tmp_path / "e.csv"
    path.write_text('s,d,label\n1,2,"a\\"b"\n2,3,plain\n')
    got = ur.read_edges(str(path), src="s", dst="d", escape="\\").to_arrow()
    assert got.column("label").to_pylist() == ['a"b', "plain"]


TYPOS = [
    (ur.scan_edges, {"src": "s", "dst": "d"}, "delimter", "delimiter"),
    (ur.read_edges, {"src": "s", "dst": "d"}, "null_valu", "null_value"),
    (ur.scan_edges, {"src": "s", "dst": "d"}, "on_nul", "on_null"),
    (ur.scan_edges, {"src": "s", "dst": "d"}, "storage_option", "storage_options"),
    (ur.read_nodes, {"id": "s"}, "has_headers", "has_header"),
    (ur.scan_nodes, {"id": "s"}, "comments", "comment"),
]


@pytest.mark.parametrize(("fn", "kw", "typo", "option"), TYPOS)
def test_a_misspelled_option_suggests_the_closest(fn, kw, typo, option):
    msg = (
        rf"^{fn.__name__}\(\) got an unexpected keyword argument '{typo}'; "
        rf"did you mean {option}=\?$"
    )
    with pytest.raises(TypeError, match=msg):
        opts: dict[str, Any] = {**kw, typo: ";"}
        fn("e.csv", **opts)


def test_a_node_scan_does_not_suggest_an_edge_option():
    # `on_null` is an edge scan's option: a node scan names no close match.
    with pytest.raises(TypeError, match=r"argument 'on_nul'\.$"):
        ur.scan_nodes("e.csv", id="s", on_nul="drop")
