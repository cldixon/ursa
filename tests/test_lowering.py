"""Relational steps lower to one DataFusion plan, in written order (#191).

Every relational step runs in the engine, wherever it sits in a pipeline, with
one set of semantics: the plain frame's. These tests pin the limits that lifts
(two ``with_columns``, ``with_columns`` after another step, a tail after
``describe()``, ``sort`` by an expression, ``select`` of an expression), the
canonical float sort order (#201), and that an engine frame gives the rows a
plain frame over the same input gives, aggregations included, bit for bit.
"""

from __future__ import annotations

import math
import struct

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

col = ur.col
with np.errstate(invalid="ignore"):
    NEG_NAN = float((np.array([0.0]) / np.array([0.0]))[0])  # 0/0: the sign bit is set on x86

EDGES = pa.table({"s": [1, 2, 3, 4, 5, 6], "d": [2, 3, 1, 1, 1, 2]})
NODES = pa.table(
    {"id": [1, 2, 3, 4, 5, 6], "c": ["x", "y", "x", "y", "x", "z"], "v": [3, 1, 2, 5, 4, 6]}
)


def _graph() -> ur.EdgeFrame:
    return ur.from_arrow(EDGES, src="s", dst="d")


def _bits(values: list) -> list:
    return [None if v is None else struct.pack("<d", v).hex() for v in values]


# --- #201: float sort keys are canonical ------------------------------------

FLOATS = pa.table(
    {
        "id": [0, 1, 2, 3, 4, 5, 6, 7],
        "r": [NEG_NAN, 1.0, math.nan, -0.0, None, 0.0, -1.0, NEG_NAN],
    }
)


def _float_frames(tmp_path) -> dict:
    path = tmp_path / "floats.parquet"
    pq.write_table(FLOATS, path, row_group_size=3)
    keys = ur.from_arrow(pa.table({"id": list(range(8))}), id="id")
    e = ur.from_arrow(
        pa.table({"s": list(range(8)), "d": [1, 2, 3, 0, 5, 6, 7, 4]}), src="s", dst="d"
    )
    return {
        "plain": ur.from_arrow(FLOATS, id="id"),
        "scan": ur.scan_nodes(str(path), id="id"),
        "join": ur.from_arrow(FLOATS, id="id").join(keys, on="id"),
        "graph": ur.from_arrow(FLOATS, id="id").with_columns(dg=ur.degree(e)),
    }


def test_sort_puts_every_nan_above_every_number(tmp_path):
    # #201: a NaN with the sign bit set used to sort first on the engine path. Now
    # every NaN is equal and above every number, and -0.0 equals 0.0, on every
    # path; ties (the NaNs, the zeros) keep input order and nulls come last.
    for name, frame in _float_frames(tmp_path).items():
        asc = frame.sort("r").collect().to_arrow().column("id").to_pylist()
        assert asc == [6, 3, 5, 1, 0, 2, 7, 4], name
        desc = frame.sort("r", descending=True).collect().to_arrow().column("id").to_pylist()
        assert desc == [0, 2, 7, 1, 3, 5, 6, 4], name
        # The same order with a second key and as a top-k.
        two = frame.sort(["r", "id"]).collect().to_arrow().column("id").to_pylist()
        assert two == asc, name
        top = frame.sort("r", descending=True).head(4).collect().to_arrow()
        assert top.column("id").to_pylist() == desc[:4], name


def test_sort_agrees_with_filters_on_nan(tmp_path):
    # docs/semantics.md: NaN is greater than every number, in filters and sorts.
    frame = ur.from_arrow(FLOATS, id="id")
    above = frame.filter(col("r") > 1.0).collect().to_arrow().column("id").to_pylist()
    tail = frame.sort("r").collect().to_arrow().column("id").to_pylist()
    assert tail[-1 - len(above) : -1] == above  # the rows above 1.0, before the null


# --- with_columns -----------------------------------------------------------


def test_two_with_columns_steps():
    g = _graph()
    one = g.nodes().with_columns(deg=ur.degree(g), pr=ur.pagerank(g))
    two = g.nodes().with_columns(deg=ur.degree(g)).with_columns(pr=ur.pagerank(g))
    # The same rows; a column attached to existing rows by id is nullable.
    assert two.collect().to_dicts() == one.collect().to_dicts()
    attrs = ur.from_arrow(NODES, id="id")
    one = attrs.with_columns(deg=ur.degree(g), pr=ur.pagerank(g))
    two = attrs.with_columns(deg=ur.degree(g)).with_columns(pr=ur.pagerank(g))
    assert two.collect().to_arrow().equals(one.collect().to_arrow())


def test_with_columns_after_other_steps_on_a_graph_frame():
    g = _graph()
    frame = g.nodes().with_columns(deg=ur.degree(g, direction="in"))
    out = frame.filter(col("deg") > 0).sort("deg").with_columns(out=ur.degree(g))
    assert out.collect().to_dicts() == [
        {"id": 3, "deg": 1, "out": 1},
        {"id": 2, "deg": 2, "out": 1},
        {"id": 1, "deg": 3, "out": 1},
    ]


def test_with_columns_of_expressions():
    g = _graph()
    nodes = ur.from_arrow(NODES, id="id")
    out = nodes.with_columns(v2=col("v") * 2, big=col("v") > 3).collect().to_arrow()
    assert out.column_names == ["id", "c", "v", "v2", "big"]
    assert out.column("v2").to_pylist() == [6, 2, 4, 10, 8, 12]
    assert out.column("big").to_pylist() == [False, False, False, True, True, True]
    # An existing column is replaced in place.
    out = nodes.with_columns(v=col("v") + 100).collect().to_arrow()
    assert out.column_names == ["id", "c", "v"]
    assert out.column("v").to_pylist() == [103, 101, 102, 105, 104, 106]
    # Mixed with a graph verb, after it, and before one.
    mixed = nodes.with_columns(deg=ur.degree(g, direction="in"), w=col("v") - 1)
    assert mixed.collect().to_arrow().column_names == ["id", "c", "v", "deg", "w"]
    after = g.nodes().with_columns(deg=ur.degree(g)).with_columns(d2=col("deg") * 2)
    rows = after.collect().to_dicts()
    assert all(r["d2"] == 2 * r["deg"] for r in rows) and len(rows) == 6
    before = nodes.with_columns(w=col("v") * 10).with_columns(deg=ur.degree(g))
    assert before.collect().to_arrow().column_names == ["id", "c", "v", "w", "deg"]


def test_with_columns_needs_the_id_column_for_graph_columns():
    g = _graph()
    nodes = ur.from_arrow(NODES, id="id")
    with pytest.raises(ur.ColumnNotFoundError, match="node id column 'id'"):
        nodes.select("v").with_columns(deg=ur.degree(g)).collect()
    with pytest.raises(ur.ColumnNotFoundError, match=r"with_columns\(\) references unknown"):
        nodes.with_columns(w=col("nope") + 1).collect()


# --- sort by an expression, select of an expression --------------------------


@pytest.mark.parametrize("engine", [False, True])
def test_sort_by_an_expression(engine):
    g = _graph()
    frame = ur.from_arrow(NODES, id="id")
    if engine:
        frame = frame.with_columns(deg=ur.degree(g))
    by_expr = frame.sort(col("v") * -1).collect().to_arrow()
    by_col = frame.sort("v", descending=True).collect().to_arrow()
    assert by_expr.equals(by_col)
    # A constant key ties every row: ties keep input order.
    mixed = frame.sort(["c", col("v") * 0], descending=[True, False]).collect().to_arrow()
    assert mixed.column("id").to_pylist() == [6, 2, 4, 1, 3, 5]


def test_select_of_expressions():
    nodes = ur.from_arrow(NODES, id="id")
    out = nodes.select("id", (col("v") * 2).alias("v2"), col("v") + 1).collect().to_arrow()
    # An unaliased expression is named after its first column.
    assert out.column_names == ["id", "v2", "v"]
    assert out.column("v2").to_pylist() == [6, 2, 4, 10, 8, 12]
    assert out.column("v").to_pylist() == [4, 2, 3, 6, 5, 7]
    with pytest.raises(ur.ColumnNotFoundError, match=r"select\(\) references unknown"):
        nodes.select(col("nope") * 2).collect()


# --- one semantics: an engine frame behaves as a plain frame -----------------


def _attrs(n: int = 200) -> pa.Table:
    f = [1.5, math.nan, -0.0, 0.0, None, NEG_NAN, 2.5, 0.1, -1.0, math.inf]
    return pa.table(
        {
            "id": list(range(n)),
            "k": [i % 7 for i in range(n)],
            "f": [f[(i * 3) % len(f)] for i in range(n)],
            "g": pa.array([None if i % 11 == 0 else i / 8 for i in range(n)], pa.float32()),
        }
    )


def test_engine_aggregates_match_the_plain_frame_bit_for_bit():
    # Float sum/mean/min/max fold in input order with pyarrow's rules for NaN and
    # signed zeros, on every path.
    n = 200
    ring = pa.table({"s": list(range(n)), "d": [(i + 1) % n for i in range(n)]})
    e = ur.from_arrow(ring, src="s", dst="d")
    engine = ur.from_arrow(_attrs(n), id="id").with_columns(dg=ur.degree(e))
    plain = ur.from_arrow(engine.collect().to_arrow(), id="id")
    aggs = {
        "fs": col("f").sum(),
        "fm": col("f").mean(),
        "fmin": col("f").min(),
        "fmax": col("f").max(),
        "gs": col("g").sum(),
        "gmax": col("g").max(),
        "n": col("f").count(),
    }
    reference = (
        _attrs(n)
        .group_by("k")
        .aggregate([("f", "sum"), ("f", "mean"), ("f", "min"), ("f", "max"), ("g", "sum")])
    )
    ref = dict(zip(reference.column("k").to_pylist(), range(reference.num_rows), strict=True))
    for frame in (engine, plain, engine.filter(col("k") >= 0)):
        out = frame.group_by("k").agg(**aggs).collect().to_arrow()
        assert out.column("k").to_pylist() == list(range(7))  # first-occurrence order
        rows = [ref[k] for k in out.column("k").to_pylist()]
        for ours, theirs in [
            ("fs", "f_sum"),
            ("fm", "f_mean"),
            ("fmin", "f_min"),
            ("fmax", "f_max"),
        ]:
            want = [reference.column(theirs)[i].as_py() for i in rows]
            assert _bits(out.column(ours).to_pylist()) == _bits(want), ours
        want = [reference.column("g_sum")[i].as_py() for i in rows]
        assert _bits(out.column("gs").to_pylist()) == _bits(want)
        assert out.schema.field("n").nullable
        assert out.schema.field("gmax").type == pa.float32()


def test_engine_sample_draws_like_a_plain_frame():
    g = _graph()
    engine = g.nodes().with_columns(deg=ur.degree(g))
    plain = ur.from_arrow(engine.collect().to_arrow(), id="id")
    for seed in (None, 0, 7, -1, 2**70):
        got = engine.sample(3, seed=seed).collect().to_dicts()
        assert got == plain.sample(3, seed=seed).collect().to_dicts()


def test_stacked_distincts_and_groups_on_an_engine_frame():
    t = pa.table(
        {
            "id": list(range(8)),
            "c": pa.array(["x", "y", "x", "y", "x", "y", "x", "y"]).dictionary_encode(),
        }
    )
    ring = ur.from_arrow(
        pa.table({"s": list(range(8)), "d": [1, 2, 3, 4, 5, 6, 7, 0]}), src="s", dst="d"
    )
    frame = ur.from_arrow(t, id="id").with_columns(dg=ur.degree(ring))
    assert frame.distinct().select("c").distinct().collect().to_dicts() == [{"c": "x"}, {"c": "y"}]
    grouped = frame.group_by("c").agg(n=col("id").count()).distinct().distinct()
    assert grouped.collect().to_dicts() == [{"c": "x", "n": 4}, {"c": "y", "n": 4}]


@pytest.mark.parametrize("engine", [False, True])
def test_unknown_columns_raise_column_not_found_on_every_step(engine):
    # One class and one message shape on every frame; ColumnNotFoundError is also
    # a ValueError, as the plain frame's errors were.
    assert issubclass(ur.ColumnNotFoundError, ValueError)
    assert issubclass(ur.ColumnNotFoundError, ur.UrsaError)
    frame = ur.from_arrow(NODES, id="id")
    if engine:
        frame = frame.with_columns(deg=ur.degree(_graph()))
    steps = {
        "filter()": lambda f: f.filter(col("nope") > 1),
        "select()": lambda f: f.select("id", "nope"),
        "rename()": lambda f: f.rename({"nope": "x"}),
        "sort()": lambda f: f.sort(["v", "nope"]),
        "group_by()": lambda f: f.group_by("nope").agg(n=col("v").count()),
        "agg()": lambda f: f.group_by("c").agg(n=col("nope").count()),
        "with_columns()": lambda f: f.with_columns(w=col("nope") + 1),
    }
    for step, build in steps.items():
        with pytest.raises(ur.ColumnNotFoundError) as exc:
            build(frame).collect()
        message = str(exc.value)
        assert message.startswith(f"{step} references unknown column 'nope'; the frame has ")
        assert "'id'" in message
