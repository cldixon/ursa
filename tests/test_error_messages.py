"""The messages of user-facing errors (#214).

Each message names the step or argument, says what was wrong and what to do,
and shows nothing of the engine's internals: no DataFusion qualifier
(`?table?`), no private column (`__ursa_...`), no serde or pyo3 wording, no
DataFusion or Arrow variant prefix. `test_messages` pins each one exactly;
`test_no_message_of_the_parity_plans_leaks_internals` runs every plan of the
parity harnesses and checks every message they raise.
"""

from __future__ import annotations

import copy
import importlib
import math
import pickle
import sys
import warnings
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

col = ur.col

EDGES = pa.table({"s": [0, 1, 2, 3], "d": [1, 2, 3, 0], "w": [1.0, 2.0, 3.0, 4.0]})
NODES = pa.table({"id": [0, 1, 2, 3], "age": [3, 4, 5, 6], "name": ["a", "b", "c", "d"]})
NEG = EDGES.set_column(2, "w", pa.array([-1.0, 1.0, 1.0, 1.0]))
HAS = "['id', 'age', 'name']"
EHAS = "['s', 'd', 'w']"

# Text no user-facing message may contain (`benchmarks/plan_matrix/parity_214.py`
# checks the same list).
LEAKS = (
    "?table?",
    "__ursa_",
    "invalid type:",
    "expected a string",
    "at line ",
    "Arrow error:",
    "Error during planning:",
    "Execution error:",
    "This feature is not implemented:",
    "Schema error:",
    "Internal error:",
    "invalid plan:",
)


def bad(value: Any) -> Any:
    """A value of a type the argument does not take, as the type checker sees it."""
    return value


def e() -> ur.EdgeFrame:
    return ur.from_arrow(EDGES, src="s", dst="d")


def n() -> ur.NodeFrame:
    return ur.from_arrow(NODES, id="id")


def run(build):
    """Build a frame and collect it (or call `build` for its side effect)."""
    out = build()
    if hasattr(out, "collect") and not isinstance(out, ur.MaterializedFrame):
        out.collect()


# (build, exception class, message). The message is exact.
CASES = {
    # --- expressions ---------------------------------------------------------
    "compare_types": (
        lambda: n().filter(col("name") > 1),
        ur.ComputeError,
        "cannot compare string with int64 (string > int64); compare values of one type",
    ),
    "compare_list": (
        lambda: n().filter(col("age") == [1, 2]),
        ur.ComputeError,
        "cannot compare int64 with list<item: int64> (int64 == list<item: int64>); "
        "compare values of one type",
    ),
    "arithmetic_types": (
        lambda: n().filter((col("name") + 1) > 1),
        ur.ComputeError,
        "cannot apply '+' to string and int64 (string + int64); arithmetic needs numeric operands",
    ),
    "arithmetic_types_select": (
        lambda: n().select((col("name") * 2).alias("x")),
        ur.ComputeError,
        "cannot apply '*' to string and int64 (string * int64); arithmetic needs numeric operands",
    ),
    "and_not_boolean": (
        lambda: n().filter((col("age") > 1) & col("age")),
        ur.ComputeError,
        "'&' combines boolean predicates (comparisons such as ur.col('a') > 1); got bool and int64",
    ),
    "not_not_boolean": (
        lambda: n().filter(~col("age")),
        ur.ComputeError,
        "'~' negates a boolean predicate (a comparison such as ur.col('a') > 1); got int64",
    ),
    "divide_by_zero": (
        lambda: n().with_columns(x=col("age") / 0),
        ur.ComputeError,
        "division by zero in an integer expression; divide by a float (e.g. / 2.0) to get "
        "inf or NaN instead, or filter out the zero divisors first",
    ),
    "lossy_compare": (
        lambda: ur.from_arrow(pa.table({"id": [2**53 + 1]}), id="id").filter(
            col("id") == float(2**53)
        ),
        ur.ComputeError,
        "the integer 9007199254740993 does not fit the type both sides are compared as "
        "(range -9007199254740992 to 9007199254740992), so the comparison would not be "
        "exact; compare with a value of the column's type",
    ),
    "unrepresentable_literal": (
        lambda: n().filter(col("age") > object()),
        TypeError,
        "a literal of type 'object' is not supported in a filter expression; use a bool, int, "
        "float, str or None, a date, time, datetime or timedelta, a Decimal or bytes",
    ),
    "filter_not_an_expression": (
        lambda: n().filter("age"),
        AttributeError,
        "filter() takes an ursa expression, such as ur.col('x') or ur.col('x') > 1; "
        "got 'age' (str)",
    ),
    # --- relational steps ----------------------------------------------------
    "unknown_column": (
        lambda: n().sort("nope"),
        ur.ColumnNotFoundError,
        f"sort() references unknown column 'nope'; the frame has {HAS}.",
    ),
    "select_aggregation": (
        lambda: n().select(col("age").sum()),
        NotImplementedError,
        "select() takes column names or expressions over columns: ur.col(...), ur.lit(...), "
        "and arithmetic and comparisons over them; got a value of another kind (an "
        "aggregation belongs in group_by().agg(), a graph verb in with_columns()).",
    ),
    "select_nothing": (
        lambda: n().select(),
        ur.ComputeError,
        "select() needs at least one column",
    ),
    "rename_value_type": (
        lambda: n().rename(bad({"age": 1})),
        ValueError,
        "rename() maps column names to new names, both strings; got 1 (int)",
    ),
    "rename_none": (
        lambda: n().rename(bad({"age": None})),
        ValueError,
        "rename() maps column names to new names, both strings; got None (NoneType)",
    ),
    "rename_not_a_dict": (
        lambda: n().rename(bad(["age"])),
        ValueError,
        "rename() takes a dict of {old: new} column names; got ['age'] (list)",
    ),
    "rename_not_iterable": (
        lambda: n().rename(bad(None)),
        TypeError,
        "rename() takes a dict of {old: new} column names; got None (NoneType)",
    ),
    "head_not_an_int": (
        lambda: n().head(bad("x")),
        ValueError,
        "head(n=) takes an integer row count; got 'x' (str)",
    ),
    "head_none": (
        lambda: n().head(bad(None)),
        TypeError,
        "head(n=) takes an integer row count; got None (NoneType)",
    ),
    "sample_negative": (
        lambda: n().sample(-1, seed=1),
        ValueError,
        "sample(n=) must be a non-negative row count; got -1",
    ),
    "group_by_sum_of_strings": (
        lambda: n().group_by("age").agg(x=col("name").sum()),
        ur.ComputeError,
        "group_by().agg(): sum() needs a numeric column; got a string column. Aggregate a "
        "numeric column, or use count() or n_unique(), which take any column",
    ),
    "group_by_mean_of_strings": (
        lambda: n().group_by("age").agg(x=col("name").mean()),
        ur.ComputeError,
        "group_by().agg(): mean() needs a numeric column; got a string column. Aggregate a "
        "numeric column, or use count() or n_unique(), which take any column",
    ),
    "join_how_type": (
        lambda: n().join(n(), on="id", how=bad(1)),
        ValueError,
        "join(how=) must be a string; got 1",
    ),
    "join_key_types": (
        lambda: n().join(ur.from_arrow(pa.table({"id": ["a"], "z": [1]}), id="id"), on="id"),
        ur.ComputeError,
        "the string 'a' cannot be read as int64; the values being matched have different "
        "types, so cast one side to the other's type first",
    ),
    "pickle": (
        lambda: pickle.dumps(n()),
        TypeError,
        "NodeFrame objects cannot be pickled or deep-copied: a frame is a lazy plan over "
        "live data. Pickle its result instead (frame.to_arrow()).",
    ),
    "deepcopy": (
        lambda: copy.deepcopy(e()),
        TypeError,
        "EdgeFrame objects cannot be pickled or deep-copied: a frame is a lazy plan over "
        "live data. Pickle its result instead (frame.to_arrow()).",
    ),
    # --- graph columns -------------------------------------------------------
    "graph_column_on_edges": (
        lambda: e().with_columns(x=ur.degree(e())),
        NotImplementedError,
        "with_columns() of a graph verb is not supported on an edge frame; graph columns "
        "belong on the node frame: edges.nodes().with_columns(...).",
    ),
    "graph_column_reserved_id": (
        lambda: e().nodes().with_columns(id=ur.degree(e())),
        ur.ComputeError,
        "output column name 'id' is reserved (it is the node id column); give the "
        "with_columns output a different name",
    ),
    "graph_column_duplicate": (
        lambda: n().with_columns(age=ur.degree(e())),
        ur.ColumnNotFoundError,
        "the result would have two columns named 'age'; give one of them another name "
        "(rename() or a different output name)",
    ),
    "with_columns_after_walk": (
        lambda: ur.random_walk(e(), start=[0], steps=2).with_columns(x=ur.degree(e())),
        NotImplementedError,
        "with_columns() after random_walk() is not supported yet; collect() the walk and "
        "build a new frame from it with ur.from_arrow(...) first.",
    ),
    "with_columns_after_describe": (
        lambda: ur.describe(e()).with_columns(x=ur.degree(e())),
        NotImplementedError,
        "with_columns() after describe() is not supported yet; collect() the result and "
        "build a new frame from it with ur.from_arrow(...) first.",
    ),
    "neighbors_string_mean": (
        lambda: n().with_columns(x=ur.neighbors(e()).agg(col("name").mean())),
        ur.ComputeError,
        "neighbors().agg() with mean/sum/min/max needs a numeric attribute column; 'name' is "
        "a string column (only n_unique/count accept strings)",
    ),
    # --- graph verb arguments ------------------------------------------------
    "verb_direction": (
        lambda: ur.degree(e(), direction="x"),
        ur.ComputeError,
        "degree(direction='x') is not supported; use one of 'out', 'in', 'both'",
    ),
    "verb_float_param": (
        lambda: ur.pagerank(e(), damping=bad("x")),
        ur.ComputeError,
        "pagerank(damping=) must be a number; got 'x'",
    ),
    "verb_int_param": (
        lambda: ur.pagerank(e(), max_iter=-1),
        ur.ComputeError,
        "pagerank(max_iter=) must be a non-negative integer below 2**32; got -1",
    ),
    "verb_seed": (
        lambda: ur.louvain(e(), seed=-1),
        ur.ComputeError,
        "louvain(seed=) must be a non-negative integer; got -1",
    ),
    "verb_nan": (
        lambda: ur.pagerank(e(), damping=math.nan),
        ValueError,
        "pagerank(damping=) must be a finite number; got NaN",
    ),
    "verb_on_a_node_frame": (
        lambda: ur.pagerank(bad(n())),
        AttributeError,
        "pagerank() takes an EdgeFrame (the graph) as its edges; got NodeFrame",
    ),
    "weight_not_an_expression": (
        lambda: ur.pagerank(e(), weight=bad("w")),
        AttributeError,
        "pagerank(weight=) takes an ursa expression, such as ur.col('x') or ur.col('x') > 1; "
        "got 'w' (str)",
    ),
    "weight_unknown_column": (
        lambda: ur.pagerank(e(), weight=col("nope")),
        ur.ColumnNotFoundError,
        f"weight expression references unknown edge column(s): ['nope']; the edges have {EHAS}",
    ),
    "weight_aggregation": (
        lambda: ur.pagerank(e(), weight=col("w").sum()),
        ur.ComputeError,
        "an aggregation (.sum(), .mean(), ...) is not allowed here; aggregations belong in "
        "group_by().agg()",
    ),
    "weight_string": (
        lambda: ur.pagerank(e().with_columns(t=ur.lit("x")), weight=col("t")),
        ur.ComputeError,
        "weight expression must be numeric; it produced a column of type string (a string or "
        "other non-numeric column cannot be a weight)",
    ),
    "weight_negative": (
        lambda: ur.pagerank(ur.from_arrow(NEG, src="s", dst="d"), weight=col("w")),
        ur.ComputeError,
        "weight expression produced a negative value (-1); weights must be non-negative",
    ),
    # --- traversals ----------------------------------------------------------
    "hop_negative": (
        lambda: ur.hop(e(), -1).from_([0]),
        OverflowError,
        "hop(n=) must be a non-negative integer; got -1",
    ),
    "hop_too_large": (
        lambda: ur.hop(e(), 2**40).from_([0]),
        OverflowError,
        "hop(n=) is too large; it must be below 2**32",
    ),
    "hop_not_an_int": (
        lambda: ur.hop(e(), bad("x")).from_([0]),
        TypeError,
        "hop(n=) must be an integer; got a str",
    ),
    "hop_direction": (
        lambda: ur.hop(e(), 1, direction="x").from_([0]),
        ur.ComputeError,
        "hop(direction='x') is not supported; use one of 'out', 'in', 'both'",
    ),
    "hop_direction_type": (
        lambda: ur.hop(e(), 1, direction=bad(1)).from_([0]),
        ValueError,
        "hop(direction=) must be a string; got 1",
    ),
    "hop_seed_type": (
        lambda: ur.hop(e(), 1).from_(["a"]),
        ur.ComputeError,
        "the node ids given are string but the graph's node ids are int64; pass ids of the "
        "graph's id type",
    ),
    "hop_not_edges": (
        lambda: ur.hop(bad(5), 1).from_([0]),
        AttributeError,
        "hop() takes an EdgeFrame (the graph) as its edges; got int",
    ),
    "shortest_path_direction": (
        lambda: ur.shortest_path(e(), 0, 1, direction="x"),
        ur.ComputeError,
        "shortest_path(direction='x') is not supported; use one of 'out', 'in', 'both'",
    ),
    "walk_steps": (
        lambda: ur.random_walk(e(), start=[0], steps=-1),
        OverflowError,
        "random_walk(steps=) must be a non-negative integer; got -1",
    ),
    "walk_seed": (
        lambda: ur.random_walk(e(), start=[0], steps=2, seed=-1),
        OverflowError,
        "random_walk(seed=) must be a non-negative integer; got -1",
    ),
    "density_not_edges": (
        lambda: ur.density(bad(5)),
        AttributeError,
        "density() takes an EdgeFrame (the graph) as its edges; got int",
    ),
    # --- sources -------------------------------------------------------------
    "src_not_a_name": (
        lambda: ur.from_arrow(EDGES, src=bad(1), dst="d"),
        TypeError,
        "src= must be a column name (a str); got 1 (int)",
    ),
    "id_not_a_name": (
        lambda: ur.from_arrow(NODES, id=bad(1)),
        TypeError,
        "id= must be a column name (a str); got 1 (int)",
    ),
    "src_missing": (
        lambda: ur.from_arrow(EDGES, src="nope", dst="d"),
        KeyError,
        f"\"src column 'nope' is not in the table; it has {EHAS}\"",
    ),
    "id_missing": (
        lambda: ur.from_arrow(NODES, id="nope"),
        KeyError,
        f"\"id column 'nope' is not in the table; it has {HAS}\"",
    ),
}


@pytest.mark.parametrize("case", sorted(CASES))
def test_messages(case):
    build, cls, message = CASES[case]
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        with pytest.raises(cls) as info:
            run(build)
    assert str(info.value) == message
    assert not [t for t in LEAKS if t in message]


def scan_cases(tmp: Path) -> dict:
    """Scan errors, whose messages name the scan and its path."""
    pq.write_table(EDGES, tmp / "e.parquet")
    pq.write_table(NODES, tmp / "n.parquet")
    (tmp / "bad.parquet").write_bytes(b"not parquet at all")
    (tmp / "bad.csv").write_text('s,d\n1,2\n"x')
    p = lambda name: str(tmp / name)  # noqa: E731
    return {
        "missing": (
            lambda: ur.scan_edges(p("missing.parquet"), src="s", dst="d"),
            ur.ComputeError,
            f"scan_edges('{p('missing.parquet')}'): no file matches the path; check it (a glob "
            "must match at least one file)",
        ),
        "missing_nodes": (
            lambda: ur.scan_nodes(p("missing.parquet"), id="id"),
            ur.ComputeError,
            f"scan_nodes('{p('missing.parquet')}'): no file matches the path; check it (a glob "
            "must match at least one file)",
        ),
        "src_missing": (
            lambda: ur.degree(ur.scan_edges(p("e.parquet"), src="nope", dst="d")),
            ur.ColumnNotFoundError,
            f"scan_edges('{p('e.parquet')}'): src column 'nope' not found in the file; it has "
            f"{EHAS}",
        ),
        "id_missing": (
            lambda: ur.scan_nodes(p("n.parquet"), id="nope"),
            ur.ColumnNotFoundError,
            f"scan_nodes('{p('n.parquet')}'): id column 'nope' not found in the file; it has {HAS}",
        ),
        "src_not_a_name": (
            lambda: ur.scan_edges(p("e.parquet"), src=bad(1), dst="d"),
            TypeError,
            "scan_edges(src=) must be a column name (a str); got 1 (int)",
        ),
        "extension": (
            lambda: ur.scan_edges(p("e.txt"), src="s", dst="d"),
            ur.ComputeError,
            f"scan_edges('{p('e.txt')}'): only .parquet and .csv files can be scanned; name a "
            "file (or a glob) with one of those extensions",
        ),
        "scheme": (
            lambda: ur.scan_edges("ftp://x/y.parquet", src="s", dst="d"),
            ur.ComputeError,
            "scan_edges('ftp://x/y.parquet'): unsupported URL scheme 'ftp'; use a local path or "
            "a file://, s3://, gs://, az://, http:// or https:// URL",
        ),
        "corrupt": (
            lambda: ur.scan_edges(p("bad.parquet"), src="s", dst="d"),
            ur.ComputeError,
            f"scan_edges('{p('bad.parquet')}'): the file is not valid Parquet (corrupt footer)",
        ),
        "csv": (
            lambda: ur.scan_edges(p("bad.csv"), src="s", dst="d"),
            ur.ComputeError,
            f"scan_edges('{p('bad.csv')}'): could not read the file as CSV: encountered "
            "unterminated string",
        ),
        "storage_options": (
            lambda: ur.scan_edges(
                "s3://bucket/x.parquet", src="s", dst="d", storage_options={"region": 1}
            ),
            TypeError,
            "storage_options= must be a dict of option names to string values; got {'region': 1}",
        ),
    }


def test_scan_messages(tmp_path):
    for case, (build, cls, message) in scan_cases(tmp_path).items():
        with pytest.raises(cls) as info:
            run(build)
        assert str(info.value) == message, case
        assert not [t for t in LEAKS if t in message], case


def test_an_engine_error_keeps_the_engine_text_as_its_cause():
    with pytest.raises(ur.ComputeError) as info:
        n().with_columns(x=col("age") / 0).collect()
    cause = info.value.__cause__
    assert isinstance(cause, RuntimeError)
    assert "Divide by zero" in str(cause)
    # A message that needed no rewording has no cause.
    with pytest.raises(ur.ColumnNotFoundError) as info:
        n().sort("nope")
    assert info.value.__cause__ is None


PLAN_MATRIX = Path(__file__).resolve().parent.parent / "benchmarks" / "plan_matrix"


def test_no_message_of_the_parity_plans_leaks_internals(tmp_path):
    """Every plan of the parity harnesses (#191, #193): no error message any of
    them raises shows the engine's internals."""
    sys.path.insert(0, str(PLAN_MATRIX))
    try:
        parity_191 = importlib.import_module("parity_191")
        parity_191_joins = importlib.import_module("parity_191_joins")
        parity_191_reshaped = importlib.import_module("parity_191_reshaped")
    finally:
        sys.path.remove(str(PLAN_MATRIX))
    messages: dict[str, str] = {}
    with warnings.catch_warnings():
        warnings.simplefilter("ignore")
        for name, harness in [
            ("parity_191", parity_191),
            ("parity_191_joins", parity_191_joins),
            ("parity_191_reshaped", parity_191_reshaped),
        ]:
            out = tmp_path / f"{name}.jsonl"
            harness.run_all(str(out))
            for rec in parity_191.load(str(out)).values():
                if "message" in rec:
                    messages.setdefault(rec["message"], rec["case"])
    assert messages, "the harnesses raise errors"
    leaking = {m: c for m, c in messages.items() if any(t in m for t in LEAKS)}
    assert not leaking
