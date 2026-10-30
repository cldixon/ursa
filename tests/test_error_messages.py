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
import decimal
import importlib
import math
import pickle
import sys
import warnings
from pathlib import Path
from typing import Any

import numpy as np
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
    "Cast error",
    "Parquet error",
    "Invalid argument error",
    "Object Store error",
    "External error",
    "caused by",
    "Cannot coerce",
    "Cannot infer common argument",
    "failed to match any signature",
    "No field named",
    "Utf8",
    "Timestamp(",
    "Dictionary(",
    "Decimal128(",
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
        "a rename() column name must be a string; got 1",
    ),
    "rename_none": (
        lambda: n().rename(bad({"age": None})),
        ValueError,
        "a rename() column name must be a string; got None",
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
        "sum() needs a numeric column, and got a string column; sum() and mean() take numbers "
        "only, while min(), max(), count() and n_unique() take strings too",
    ),
    "group_by_mean_of_strings": (
        lambda: n().group_by("age").agg(x=col("name").mean()),
        ur.ComputeError,
        "mean() needs a numeric column, and got a string column; sum() and mean() take numbers "
        "only, while min(), max(), count() and n_unique() take strings too",
    ),
    "join_how_type": (
        lambda: n().join(n(), on="id", how=bad(1)),
        ValueError,
        "join(how=) must be a string; got 1",
    ),
    "join_key_types": (
        lambda: n().join(ur.from_arrow(pa.table({"id": ["a"], "z": [1]}), id="id"), on="id"),
        ur.ComputeError,
        "join(on=): the left and right keys have different types (the string 'a' cannot be "
        "read as int64); cast one side's key to the other's type first",
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
    # --- #217: expressions, describe and the stats --------------------------
    "expression_truth_value": (
        lambda: bool(col("age") == 1),
        TypeError,
        "the truth value of an ursa expression is ambiguous: `==` and the other comparisons "
        "build an expression rather than compare. Combine predicates with &, | and ~ instead "
        "of and, or and not, and compare collected results.",
    ),
    "describe_nodes": (
        lambda: ur.describe(e(), nodes=bad("x")),
        ValueError,
        "describe(nodes='x') is not supported; use 'all' or 'touched'.",
    ),
    "stat_direction": (
        lambda: ur.density(e(), direction="x", lazy=True),
        ValueError,
        "density(direction='x') is not supported; use 'out', 'in', or 'both'.",
    ),
    "stat_sample": (
        lambda: ur.avg_path_length(e(), sample=bad("x")),
        TypeError,
        "avg_path_length(sample=) must be a number or None; got 'x' (str)",
    ),
    "stat_approximate": (
        lambda: ur.diameter(e(), approximate=bad("x")),
        TypeError,
        "diameter(approximate=) must be a bool; got 'x' (str)",
    ),
    "stat_then_graph_column": (
        lambda: ur.density(e(), lazy=True).with_columns(x=ur.degree(e())),
        NotImplementedError,
        "with_columns() after density() is not supported yet; collect() the result and build "
        "a new frame from it with ur.from_arrow(...) first.",
    ),
    "bare_nodes_unknown_column": (
        lambda: e().nodes().filter(col("nope") > 1),
        ur.ColumnNotFoundError,
        "filter() references unknown column 'nope'; the frame has ['id'].",
    ),
    "verb_frame_unknown_key": (
        lambda: ur.degree(e()).join(n(), on="nope"),
        ur.ColumnNotFoundError,
        "join(on=): key 'nope' not in the left frame ['id', 'degree'].",
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
    pq.write_table(EDGES.set_column(2, "w", pa.array([1, 2, 3, 4])), tmp / "e_int.parquet")
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
        # A list of paths, and the CSV options (#216).
        "list_columns": (
            lambda: ur.scan_edges([p("e.parquet"), p("n.parquet")], src="s", dst="d"),
            ur.ComputeError,
            f"scan_edges(['{p('e.parquet')}', '{p('n.parquet')}']): the paths in a list are read "
            "as one table, so each needs the same columns, with the same types, in the same "
            f"order. '{p('e.parquet')}' has the columns [s, d, w] and '{p('n.parquet')}' has "
            "[id, age, name]. Give every file the same columns, or read each with its own scan.",
        ),
        "list_types": (
            lambda: ur.scan_edges([p("e.parquet"), p("e_int.parquet")], src="s", dst="d"),
            ur.ComputeError,
            f"scan_edges(['{p('e.parquet')}', '{p('e_int.parquet')}']): the paths in a list are "
            "read as one table, so each needs the same columns, with the same types, in the "
            f"same order. Column 'w' is double in '{p('e.parquet')}' and int64 in "
            f"'{p('e_int.parquet')}'. Write the files with matching types (in a CSV file, a type "
            "is inferred from the values: write 1.0 for a float in every file), or read them "
            "into memory, cast the columns to one type and concatenate them.",
        ),
        "list_stores": (
            lambda: ur.scan_edges([p("e.parquet"), "s3://bucket/e.parquet"], src="s", dst="d"),
            ur.ComputeError,
            f"scan_edges(['{p('e.parquet')}', 's3://bucket/e.parquet']): the paths in a list "
            f"must be on one store, and '{p('e.parquet')}' and 's3://bucket/e.parquet' are not; "
            "copy the files to one store, or read them into memory",
        ),
        "list_empty": (
            lambda: ur.scan_edges([], src="s", dst="d"),
            ValueError,
            "scan_edges(): the list of paths is empty. Pass at least one path.",
        ),
        "path_type": (
            lambda: ur.read_nodes(3, id="id"),  # ty: ignore[invalid-argument-type]
            TypeError,
            "read_nodes(): a path must be a string or a pathlib.Path, or a list of them; got int.",
        ),
        "csv_delimiter": (
            lambda: ur.scan_edges(p("e.csv"), src="s", dst="d", delimiter=";;"),
            ValueError,
            "scan_edges(): delimiter must be one ASCII character; got ';;'. Pass a single "
            "character, such as ';'.",
        ),
        "csv_on_parquet": (
            lambda: ur.read_edges(p("e.parquet"), src="s", dst="d", null_value="NA"),
            ValueError,
            f"read_edges(): the CSV options (null_value=) only apply to .csv files, and "
            f"'{p('e.parquet')}' is not one. Remove them, or scan a .csv file.",
        ),
        "csv_alias": (
            lambda: ur.read_edges(p("e.csv"), src="s", dst="d", sep=";"),
            TypeError,
            "read_edges() got an unexpected keyword argument 'sep'; use delimiter= instead.",
        ),
        "csv_typo": (
            lambda: ur.scan_nodes(p("e.csv"), id="s", delimter=";"),
            TypeError,
            "scan_nodes() got an unexpected keyword argument 'delimter'; did you mean delimiter=?",
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


@pytest.mark.parametrize(
    ("build", "cls", "cause"),
    [
        # The class main raised, exactly: a rename name serde reads as a number.
        (lambda: n().rename(bad({"age": decimal.Decimal(1)})), ValueError, RuntimeError),
        (lambda: n().rename(bad({"age": np.float32(1)})), ValueError, RuntimeError),
        (lambda: n().rename(bad({"age": object()})), TypeError, TypeError),
        (lambda: n().rename(bad({"age": [object()]})), TypeError, TypeError),
        (lambda: n().head(bad("x")), ValueError, ValueError),
        (lambda: n().head(bad(b"x")), ValueError, ValueError),
        (lambda: n().sample(bad(None)), TypeError, TypeError),
        (lambda: n().join(n(), on="id", how=bad(1)), ValueError, RuntimeError),
        (lambda: ur.pagerank(e(), damping=bad(object())), TypeError, TypeError),
        (lambda: n().with_columns(x=col("age") / 0), ur.ComputeError, RuntimeError),
        # A subclass is raised as it is, not reworded into its base class.
        (lambda: n().filter(col("name") == "\ud800"), UnicodeEncodeError, None),
        (lambda: n().head(bad(float("inf"))), OverflowError, None),
        (lambda: ur.avg_path_length(e(), sample=bad("x")), TypeError, TypeError),
        (lambda: ur.diameter(e(), approximate=bad("x")), TypeError, TypeError),
    ],
)
def test_exception_classes_and_causes(build, cls, cause):
    with pytest.raises(BaseException) as info:
        run(build)
    assert type(info.value) is cls
    if cause is None:
        assert info.value.__cause__ is None
    else:
        assert type(info.value.__cause__) is cause


def repros(tmp: Path) -> dict:
    """Errors from DataFusion and Arrow that a plan can reach: operand types that
    do not combine (timestamps, dictionaries, decimals, lists), aggregation
    signatures, casts, and storage."""
    t = pa.table(
        {
            "id": [0, 1, 2],
            "ts": pa.array([1, 2, 3], pa.timestamp("us")),
            "tz": pa.array([1, 2, 3], pa.timestamp("ns", "UTC")),
            "c": pa.array(["x", "y", "x"]).dictionary_encode(),
            "dec": pa.array([decimal.Decimal("1.50"), None, None], pa.decimal128(10, 2)),
            "s": ["a", "b", "c"],
            "b": [True, False, True],
            "l": pa.array([[1], [2], [3]]),
        }
    )
    f = ur.from_arrow(t, id="id")
    strs = ur.from_arrow(pa.table({"id": ["a"], "z": [1]}), id="id")
    return {
        "ts_plus_int": lambda: f.with_columns(x=col("ts") + 1),
        "ts_times_ts": lambda: f.with_columns(x=col("ts") * col("ts")),
        "tz_minus_str": lambda: f.with_columns(x=col("tz") - col("s")),
        "dict_plus_int": lambda: f.with_columns(x=col("c") + 1),
        "dec_times_dict": lambda: f.with_columns(x=col("dec") * col("c")),
        "dec_div_str": lambda: f.with_columns(x=col("dec") / col("s")),
        "list_plus_int": lambda: f.with_columns(x=col("l") + 1),
        "bool_and_dict": lambda: f.filter(col("b") & col("c")),
        "dict_or_ts": lambda: f.filter(col("c") | col("ts")),
        "not_dict": lambda: f.filter(~col("c")),
        "dict_gt_int": lambda: f.filter(col("c") > 1),
        "ts_eq_dec": lambda: f.filter(col("ts") == col("dec")),
        "sum_ts": lambda: f.group_by("s").agg(x=col("ts").sum()),
        "mean_dict": lambda: f.group_by("s").agg(x=col("c").mean()),
        "mean_list": lambda: f.group_by("s").agg(x=col("l").mean()),
        "sum_str": lambda: f.group_by("id").agg(x=col("s").sum()),
        "join_str_int": lambda: f.select("id", "s").join(strs, on="id"),
        "join_int_str": lambda: strs.join(f.select("id", "s"), on="id"),
        "attach_str_ids": lambda: strs.with_columns(dg=ur.degree(e())),
        "missing_file": lambda: ur.scan_edges(str(tmp / "nope" / "e.parquet"), src="s", dst="d"),
        "missing_glob": lambda: ur.scan_nodes(str(tmp / "*.parquet"), id="id"),
        "http_refused": lambda: ur.scan_edges("http://127.0.0.1:1/e.parquet", src="s", dst="d"),
        "http_query": lambda: ur.scan_edges("https://h.invalid/e.parquet?t=1", src="s", dst="d"),
        "s3_bad_option": lambda: ur.scan_edges(
            "s3://bucket/e.parquet", src="s", dst="d", storage_options={"not_an_option": "x"}
        ),
        "s3_bad_url": lambda: ur.scan_edges("s3://", src="s", dst="d"),
        "if_expression": lambda: 1 if col("a") > 1 else 0,
        "describe_nodes_int": lambda: ur.describe(e(), nodes=bad(1)),
        "stat_lazy_unknown": lambda: ur.avg_path_length(e(), lazy=True).select("nope"),
        "stat_lazy_compare": lambda: ur.density(e(), lazy=True).filter(col("density") > "x"),
        "stat_of_node_frame": lambda: ur.density(bad(f)),
        "bare_nodes_compare": lambda: e().nodes().filter(col("id") > "x"),
        "bare_nodes_arithmetic": lambda: e().nodes().with_columns(y=col("id") + "x"),
        "verb_frame_group": lambda: ur.degree(e()).group_by("nope").agg(x=col("id").count()),
    }


def test_repro_messages(tmp_path):
    """Each repro raises, in Ursa's words."""
    for case, build in repros(tmp_path).items():
        with pytest.raises(Exception) as info:
            run(build)
        message = str(info.value)
        assert not [t for t in LEAKS if t in message], (case, message)
    with pytest.raises(ur.ComputeError) as info:
        run(repros(tmp_path)["join_str_int"])
    assert str(info.value) == (
        "join(on=): the left and right keys have different types (the string 'a' cannot be "
        "read as int64); cast one side's key to the other's type first"
    )
    with pytest.raises(ur.ComputeError) as info:
        run(repros(tmp_path)["attach_str_ids"])
    assert str(info.value) == (
        "with_columns(): the frame's id column and the graph's node ids have different types "
        "(the string 'a' cannot be read as int64); build the frame with ids of the graph's id "
        "type"
    )
    with pytest.raises(ur.ComputeError) as info:
        run(repros(tmp_path)["dec_times_dict"])
    assert str(info.value) == (
        "cannot apply '*' to decimal128(10, 2) and dictionary<values=string, indices=int32, "
        "ordered=0> (decimal128(10, 2) * dictionary<values=string, indices=int32, ordered=0>); "
        "arithmetic needs numeric operands"
    )


PLAN_MATRIX = Path(__file__).resolve().parent.parent / "benchmarks" / "plan_matrix"


def test_no_user_facing_message_leaks_internals(tmp_path):
    """Every error the parity harnesses raise (`parity_191.py`,
    `parity_191_joins.py`, `parity_191_reshaped.py`, which run each case through
    `parity_193.py`'s runner), every pinned case above, every scan case and every
    repro: no message shows the engine's internals."""
    sys.path.insert(0, str(PLAN_MATRIX))
    try:
        parity_191 = importlib.import_module("parity_191")
        parity_191_joins = importlib.import_module("parity_191_joins")
        parity_191_reshaped = importlib.import_module("parity_191_reshaped")
    finally:
        sys.path.remove(str(PLAN_MATRIX))
    messages: dict[str, str] = {}
    harness_cases = 0
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
                    harness_cases += 1
                    messages.setdefault(rec["message"], rec["case"])
        builds = {f"case:{k}": v[0] for k, v in CASES.items()}
        builds |= {f"scan:{k}": v[0] for k, v in scan_cases(tmp_path).items()}
        builds |= {f"repro:{k}": v for k, v in repros(tmp_path).items()}
        for case, build in builds.items():
            try:
                run(build)
            except Exception as exc:
                messages.setdefault(str(exc), case)
    # Every error case of the harnesses, and every other case, was checked.
    assert harness_cases >= 1500, harness_cases
    assert len(messages) >= 100, len(messages)
    leaking = {m: c for m, c in messages.items() if any(t in m for t in LEAKS)}
    assert not leaking
