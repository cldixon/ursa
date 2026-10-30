"""Float spellings in a CSV scan (#221).

arrow-csv's type inference reads `+5`, `Inf`, `-nan` or `Infinity` as text, so
one such value in the inference sample made the whole column `string`, where
pyarrow reads `double`. A scan now types such a column float64, and every value
parses to the bits pyarrow gives. Each case runs on the default path and on the
`null_value` path, and is compared bit for bit with `pyarrow.csv` given the same
null values.
"""

from __future__ import annotations

import io
import struct
from typing import Any

import pyarrow as pa
import pyarrow.csv as pacsv
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(not ur._NATIVE_AVAILABLE, reason="native extension not built")

PATHS: dict[str, dict[str, Any]] = {"default": {}, "null_value": {"null_value": "NA"}}

# Each spelling pyarrow reads as a double. The first group is what the issue
# names; the rest are the other spellings of the same kinds.
SPELLINGS = [
    "1e400",
    "-1e400",
    "nan",
    "NaN",
    "inf",
    "-inf",
    "-0",
    "+5",
    "NAN",
    "Nan",
    "-nan",
    "+nan",
    "-NaN",
    "Inf",
    "INF",
    "+inf",
    "-Inf",
    "infinity",
    "Infinity",
    "-Infinity",
    "+Infinity",
    "+0",
    "+5.5",
    "+.5",
    "+5.",
    "+1e5",
    "-00",
    "1e-400",
    "4.9e-324",
    "1.7976931348623159e308",
]

# The other values in the column: none, a float, an integer.
WITH = {"alone": [], "float": ["1.5"], "int": ["7"]}


def _pyarrow(text: str, null_value: str | None = None) -> pa.Table:
    nulls = [""] + ([null_value] if null_value is not None else [])
    convert = pacsv.ConvertOptions(null_values=nulls, strings_can_be_null=True)
    return pacsv.read_csv(io.BytesIO(text.encode()), convert_options=convert)


def _write(tmp_path, values: list[str]) -> tuple[str, str]:
    text = "id,x\n" + "".join(f"{i},{v}\n" for i, v in enumerate(values))
    path = tmp_path / "x.csv"
    path.write_text(text)
    return str(path), text


def _bits(column: pa.ChunkedArray) -> list[Any]:
    """Each value of a column, a float as its 64 bits (so NaN's sign and -0.0
    compare), anything else as itself."""
    return [struct.pack(">d", v).hex() if isinstance(v, float) else v for v in column.to_pylist()]


def _same_as_pyarrow(got: pa.Table, want: pa.Table) -> None:
    assert got.schema == want.schema
    for name in want.column_names:
        assert _bits(got.column(name)) == _bits(want.column(name)), name


@pytest.mark.parametrize("path", sorted(PATHS))
@pytest.mark.parametrize("context", sorted(WITH))
@pytest.mark.parametrize("value", SPELLINGS)
def test_a_float_spelling_reads_as_pyarrow_reads_it(tmp_path, value, context, path):
    opts = PATHS[path]
    file, text = _write(tmp_path, [value, *WITH[context], ""])
    got = ur.read_nodes(file, id="id", **opts).to_arrow()
    want = _pyarrow(text, opts.get("null_value"))
    _same_as_pyarrow(got, want)


@pytest.mark.parametrize("path", sorted(PATHS))
def test_the_issue_repro_reads_as_pyarrow_reads_it(tmp_path, path):
    opts = PATHS[path]
    file, text = _write(tmp_path, ["1e400", "-0", "nan", "+5", "1.5"])
    got = ur.scan_nodes(file, id="id", **opts).collect().to_arrow()
    assert got.schema.field("x").type == pa.float64()
    assert _bits(got.column("x")) == [
        "7ff0000000000000",
        "8000000000000000",
        "7ff8000000000000",
        "4014000000000000",
        "3ff8000000000000",
    ]
    _same_as_pyarrow(got, _pyarrow(text, opts.get("null_value")))


@pytest.mark.parametrize("path", sorted(PATHS))
def test_the_null_value_reads_as_null_next_to_a_float_spelling(tmp_path, path):
    file, _ = _write(tmp_path, ["+5", "NA", "Inf"])
    got = ur.read_nodes(file, id="id", null_value="NA").to_arrow()
    assert got.schema.field("x").type == pa.float64()
    assert got.column("x").to_pylist() == [5.0, None, float("inf")]


# Columns whose type does not change: what arrow-csv infers, it still infers.
UNCHANGED = {
    "text_and_plus": (["+5", "abc"], pa.string()),
    "date_and_plus": (["+5", "2020-01-01"], pa.string()),
    "bool_and_plus": (["+5", "true"], pa.string()),
    "int_past_int64": (["9223372036854775808", "1"], pa.string()),
    "space": ([" 5", "1.5"], pa.string()),
    "ints": (["1", "-0", "-00"], pa.int64()),
    "floats": (["1.5", "nan", "-inf", "1e400"], pa.float64()),
    "nulls": (["", ""], pa.null()),
}


@pytest.mark.parametrize("path", sorted(PATHS))
@pytest.mark.parametrize("case", sorted(UNCHANGED))
def test_a_column_arrow_csv_types_keeps_its_type(tmp_path, case, path):
    values, kind = UNCHANGED[case]
    file, _ = _write(tmp_path, values)
    got = ur.read_nodes(file, id="id", **PATHS[path]).to_arrow()
    assert got.schema.field("x").type == kind


SAMPLE = 1000  # DataFusion's inference sample, in rows


@pytest.mark.parametrize("path", sorted(PATHS))
def test_a_spelling_past_the_sample_parses_as_the_sampled_type(tmp_path, path):
    # The sample infers float64, and the reader parses `+5` and `Inf` past it.
    file, text = _write(tmp_path, ["1.5"] * SAMPLE + ["+5", "Inf", "-nan"])
    got = ur.read_nodes(file, id="id", **PATHS[path]).to_arrow()
    _same_as_pyarrow(got, _pyarrow(text))
    # An int64 sample stays int64: `+5` past it parses as the integer 5, as
    # before.
    file, _ = _write(tmp_path, ["1"] * SAMPLE + ["+5"])
    got = ur.read_nodes(file, id="id", **PATHS[path]).to_arrow()
    assert got.schema.field("x").type == pa.int64()
    assert got.column("x").to_pylist()[-1] == 5


@pytest.mark.parametrize("path", sorted(PATHS))
def test_a_spelling_only_past_the_sample_does_not_change_the_type(tmp_path, path):
    # Only the sample sets the type: a text value past it fails to parse with
    # the same message on both paths.
    file, _ = _write(tmp_path, ["+5"] * SAMPLE + ["abc"])
    with pytest.raises(ur.ComputeError) as plain:
        ur.read_nodes(file, id="id").to_arrow()
    with pytest.raises(ur.ComputeError) as other:
        ur.read_nodes(file, id="id", **PATHS[path]).to_arrow()
    assert str(other.value) == str(plain.value)
    assert "'abc'" in str(plain.value)


def test_each_file_of_a_glob_and_each_path_of_a_list_reads_float64(tmp_path):
    (tmp_path / "a.csv").write_text("id,x\n0,+5\n1,Inf\n")
    (tmp_path / "b.csv").write_text("id,x\n2,1.5\n3,-nan\n")
    want = pa.concat_tables([_pyarrow((tmp_path / n).read_text()) for n in ("a.csv", "b.csv")])
    for paths in (str(tmp_path / "*.csv"), [str(tmp_path / "a.csv"), str(tmp_path / "b.csv")]):
        got = ur.read_nodes(paths, id="id").to_arrow().sort_by("id")
        _same_as_pyarrow(got, want)


def _outcome(fn):
    """The result's table, or the error's class and message."""
    try:
        return fn().collect().to_arrow()
    except Exception as e:  # the outcome is compared, whatever it is
        return (type(e).__name__, str(e))


WEIGHTED = {
    "pagerank": lambda e: ur.pagerank(e, weight=ur.col("w")),
    "closeness": lambda e: e.nodes().with_columns(c=ur.closeness(e, weight=ur.col("w"))),
    "shortest_path": lambda e: ur.shortest_path(e, 0, 3, weight=ur.col("w")),
}


@pytest.mark.parametrize("path", sorted(PATHS))
@pytest.mark.parametrize("op", sorted(WEIGHTED))
def test_a_weighted_algorithm_over_a_csv_weight_with_nan(tmp_path, op, path):
    # The weight column has `+5` and `nan`: it was `string` and failed with a
    # type error. Now it reads as pyarrow reads it, and the algorithm gives
    # what it gives over that table in memory.
    text = "s,d,w\n0,1,+5\n1,2,nan\n2,3,1.5\n3,0,2\n0,2,+0.5\n"
    file = tmp_path / "e.csv"
    file.write_text(text)
    scanned = ur.scan_edges(str(file), src="s", dst="d", **PATHS[path])
    assert scanned.collect().to_arrow().schema.field("w").type == pa.float64()
    memory = ur.EdgeFrame(_pyarrow(text), src="s", dst="d")
    got, want = _outcome(lambda: WEIGHTED[op](scanned)), _outcome(lambda: WEIGHTED[op](memory))
    if isinstance(want, pa.Table):
        assert isinstance(got, pa.Table), got
        _same_as_pyarrow(got, want)
    else:
        assert got == want
