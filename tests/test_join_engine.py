"""Joins in the engine (#191, part 2): any number of joins, anywhere in a plan,
over any frame, with every ``how``.

Each result is checked against a nested-loop join of the two sides' collected
rows, which pins the row order each ``how`` guarantees:

- inner/left/semi/anti: the left rows in order, each followed by its matches in
  right-frame order;
- right: the right rows in order, each followed by its matches in left order;
- outer: left's order, then the unmatched right rows in right-frame order.
"""

from __future__ import annotations

import math

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

HOWS = ["inner", "left", "right", "outer", "semi", "anti"]


def _rows(t: pa.Table) -> list[tuple]:
    cols = [c.to_pylist() for c in t.columns]
    return list(zip(*cols, strict=True))


def _eq(a, b) -> bool:
    return a is not None and b is not None and a == b


def nested_loop(left: pa.Table, right: pa.Table, on: list[str], how: str) -> list[tuple]:
    """The join of two collected tables, in the order the join guarantees."""
    lk = [left.column_names.index(k) for k in on]
    rk = [right.column_names.index(k) for k in on]
    rv = [i for i, c in enumerate(right.column_names) if c not in on]
    lrows, rrows = _rows(left), _rows(right)

    def match(lr, rr):
        return all(_eq(lr[a], rr[b]) for a, b in zip(lk, rk, strict=True))

    def joined(lr, rr):
        if lr is None:  # a right-only row: the keys take the right key
            keys = dict(zip(lk, (rr[b] for b in rk), strict=True))
            lr = tuple(keys.get(i) for i in range(left.num_columns))
        right_vals = tuple(None if rr is None else rr[i] for i in rv)
        return tuple(lr) + right_vals

    out = []
    if how == "right":
        for rr in rrows:
            ms = [lr for lr in lrows if match(lr, rr)]
            out.extend(joined(lr, rr) for lr in ms) if ms else out.append(joined(None, rr))
        return out
    for lr in lrows:
        ms = [rr for rr in rrows if match(lr, rr)]
        if how == "semi":
            out.extend([tuple(lr)] if ms else [])
        elif how == "anti":
            out.extend([] if ms else [tuple(lr)])
        elif ms:
            out.extend(joined(lr, rr) for rr in ms)
        elif how in ("left", "outer"):
            out.append(joined(lr, None))
    if how == "outer":
        out.extend(joined(None, rr) for rr in rrows if not any(match(lr, rr) for lr in lrows))
    return out


def check(left, right, on, how, *, ref_left=None, ref_right=None):
    """Join `left` and `right` in the engine and compare with the nested loop
    over their collected rows (or `ref_left` / `ref_right`, when given)."""
    on = [on] if isinstance(on, str) else on
    lt = (ref_left if ref_left is not None else left).collect().to_arrow()
    rt = (ref_right if ref_right is not None else right).collect().to_arrow()
    out = left.join(right, on=on, how=how).collect().to_arrow()
    want_cols = lt.column_names + (
        [] if how in ("semi", "anti") else [c for c in rt.column_names if c not in on]
    )
    assert out.column_names == want_cols
    assert _rows(out) == nested_loop(lt, rt, on, how)
    return out


def edges():
    return ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2, 2, 3, 4, 5], "d": [1, 2, 2, 0, 3, 4, 5, 3]}), src="s", dst="d"
    )


def attrs():
    return ur.from_arrow(
        pa.table(
            {
                "id": [3, 1, 4, 1, 5, 9, 2, 6, 0, None],
                "v": [10, 11, 12, 13, 14, 15, 16, 17, 18, 19],
            }
        ),
        id="id",
    )


def other():
    return ur.from_arrow(
        pa.table({"id": [5, 1, None, 7, 1, 3, 3], "w": [1.5, 2.5, 3.5, 4.5, 5.5, 6.5, 7.5]}),
        id="id",
    )


# --- every how, and its order ----------------------------------------------


@pytest.mark.parametrize("how", HOWS)
def test_every_how_follows_its_order(how):
    # Duplicate keys on both sides (many-to-many), a null key on each side, and
    # keys only one side has.
    check(attrs(), other(), "id", how)


@pytest.mark.parametrize("how", HOWS)
def test_every_how_on_a_large_frame(how):
    # Large enough that the hash join spreads over every partition.
    n = 20_000
    left = ur.from_arrow(
        pa.table({"id": [(i * 7919) % 5003 for i in range(n)], "a": list(range(n))}), id="id"
    )
    right = ur.from_arrow(
        pa.table({"id": [(i * 104729) % 6007 for i in range(n // 2)], "b": list(range(n // 2))}),
        id="id",
    )
    check(left, right, "id", how)


def test_right_and_outer_keys_take_the_right_key_where_the_left_has_none():
    left = ur.from_arrow(pa.table({"id": [1, 2], "a": ["x", "y"]}), id="id")
    right = ur.from_arrow(pa.table({"id": [3, 2, 4], "b": [30, 20, 40]}), id="id")
    assert left.join(right, on="id", how="right").collect().to_dicts() == [
        {"id": 3, "a": None, "b": 30},
        {"id": 2, "a": "y", "b": 20},
        {"id": 4, "a": None, "b": 40},
    ]
    assert left.join(right, on="id", how="outer").collect().to_dicts() == [
        {"id": 1, "a": "x", "b": None},
        {"id": 2, "a": "y", "b": 20},
        {"id": 3, "a": None, "b": 30},
        {"id": 4, "a": None, "b": 40},
    ]
    assert left.join(right, on="id", how="full").collect().to_dicts() == (
        left.join(right, on="id", how="outer").collect().to_dicts()
    )


def test_semi_and_anti_keep_left_rows_once():
    left = ur.from_arrow(pa.table({"id": [1, 2, None, 1, 4], "a": [0, 1, 2, 3, 4]}), id="id")
    right = ur.from_arrow(pa.table({"id": [1, 1, None, 4], "a": [9, 9, 9, 9]}), id="id")
    # A same-named column on the right is fine: semi/anti keep the left columns.
    semi = left.join(right, on="id", how="semi").collect().to_dicts()
    assert semi == [{"id": 1, "a": 0}, {"id": 1, "a": 3}, {"id": 4, "a": 4}]
    anti = left.join(right, on="id", how="anti").collect().to_dicts()
    assert anti == [{"id": 2, "a": 1}, {"id": None, "a": 2}]


@pytest.mark.parametrize("how", HOWS)
def test_multi_key_join(how):
    left = ur.from_arrow(
        pa.table({"id": [0, 0, 1, 1, None], "k": ["x", "y", "x", None, "x"], "a": [1, 2, 3, 4, 5]}),
        id="id",
    )
    right = ur.from_arrow(
        pa.table({"id": [1, 0, 0, 2, 1], "k": ["x", "x", "x", "z", None], "b": [6, 7, 8, 9, 10]}),
        id="id",
    )
    check(left, right, ["id", "k"], how)


@pytest.mark.parametrize("how", HOWS)
def test_string_and_dictionary_keys(how):
    left = ur.from_arrow(
        pa.table({"id": [0, 1, 2, 3], "c": pa.array(["b", "a", None, "b"]).dictionary_encode()}),
        id="id",
    )
    right = ur.from_arrow(
        pa.table({"c": pa.array(["a", "b", "c", "a"]).dictionary_encode(), "z": [1, 2, 3, 4]}),
        id="c",
    )
    out = check(left, right, "c", how)
    assert out.schema.field("c").type == pa.dictionary(pa.int32(), pa.string())
    s = ur.from_arrow(pa.table({"s": ["u", "v", None], "a": [1, 2, 3]}), id="s")
    t = ur.from_arrow(pa.table({"s": ["v", "w", "u", "v"], "b": [4, 5, 6, 7]}), id="s")
    check(s, t, "s", how)


def test_null_keys_never_match():
    left = ur.from_arrow(pa.table({"id": [None, 1], "a": [1, 2]}), id="id")
    right = ur.from_arrow(pa.table({"id": [None, 1], "b": [3, 4]}), id="id")
    assert left.join(right, on="id").collect().to_dicts() == [{"id": 1, "a": 2, "b": 4}]
    assert left.join(right, on="id", how="outer").collect().to_dicts() == [
        {"id": None, "a": 1, "b": None},
        {"id": 1, "a": 2, "b": 4},
        {"id": None, "a": None, "b": 3},
    ]


def test_float_keys_with_nan_and_signed_zero():
    nan = float("nan")
    left = ur.from_arrow(pa.table({"f": [0.0, -0.0, nan, 1.5, None], "a": [1, 2, 3, 4, 5]}), id="f")
    right = ur.from_arrow(pa.table({"f": [nan, 0.0, 1.5, -0.0], "b": [6, 7, 8, 9]}), id="f")
    for how in HOWS:
        out = left.join(right, on="f", how=how).collect().to_arrow()
        # The order holds; which float keys match is the hash join's equality.
        assert out.num_rows == len(_rows(out))
    inner = left.join(right, on="f").collect().to_dicts()
    assert [r["a"] for r in inner] == sorted(r["a"] for r in inner)
    assert {(r["a"], r["b"]) for r in inner} >= {(4, 8)}
    assert not any(math.isnan(r["f"]) and r["a"] == 5 for r in inner)


@pytest.mark.parametrize("how", ["inner", "left", "right", "outer"])
def test_collisions_keep_their_message(how):
    with pytest.raises(ValueError, match=r"duplicate column name\(s\) \['v'\]"):
        attrs().join(attrs(), on="id", how=how).collect()


def test_unknown_how_errors():
    with pytest.raises(NotImplementedError, match=r"join how='cross' is not supported \(use"):
        attrs().join(other(), on="id", how="cross").collect()


# --- any number of joins, anywhere ------------------------------------------


def third():
    return ur.from_arrow(pa.table({"id": [9, 1, 3, 1, 0], "x": ["p", "q", "r", "s", "t"]}), id="id")


@pytest.mark.parametrize("how", ["inner", "left", "right", "outer"])
@pytest.mark.parametrize("how2", ["inner", "left", "right", "outer", "semi", "anti"])
def test_two_joins(how, how2):
    first = attrs().join(other(), on="id", how=how)
    check(first, third(), "id", how2)


def test_three_joins():
    a = attrs().join(other(), on="id", how="left")
    b = a.join(third(), on="id", how="outer")
    fourth = ur.from_arrow(pa.table({"id": [1, 9, 2], "y": [True, False, True]}), id="id")
    check(b, fourth, "id", "right")
    # The right side is itself a join.
    check(third(), attrs().join(other(), on="id", how="inner"), "id", "left")


STEPS = {
    "with_columns": lambda f: f.with_columns(v2=ur.col("v") * 2),
    "filter": lambda f: f.filter(ur.col("v") > 11),
    "sort": lambda f: f.sort("v", descending=True),
    "group_by": lambda f: f.group_by("id").agg(v=ur.col("v").sum()),
    "distinct": lambda f: f.select("id", "v").distinct(),
    "head": lambda f: f.head(6),
    "sample": lambda f: f.sample(5, seed=4),
}


@pytest.mark.parametrize("step", STEPS)
@pytest.mark.parametrize("how", HOWS)
def test_join_after_a_step(step, how):
    left = STEPS[step](attrs())
    check(left, other(), "id", how)


@pytest.mark.parametrize("step", STEPS)
@pytest.mark.parametrize("how", HOWS)
def test_join_of_a_stepped_right_side(step, how):
    right = STEPS[step](attrs()).rename({"v": "rv"})
    check(other(), right, "id", how)


@pytest.mark.parametrize("step", ["filter", "sort", "head", "distinct", "group_by", "sample"])
def test_steps_after_a_join(step):
    joined = attrs().join(other(), on="id", how="outer")
    plain = ur.from_arrow(joined.collect().to_arrow(), id="id")
    build = {
        "filter": lambda f: f.filter(ur.col("w") > 2),
        "sort": lambda f: f.sort("w"),
        "head": lambda f: f.head(4),
        "distinct": lambda f: f.select("id").distinct(),
        "group_by": lambda f: f.group_by("id").agg(n=ur.col("v").count(), w=ur.col("w").sum()),
        "sample": lambda f: f.sample(4, seed=9),
    }[step]
    assert build(joined).collect().to_dicts() == build(plain).collect().to_dicts()


def test_with_columns_between_joins():
    e = edges()
    out = (
        attrs()
        .join(other(), on="id", how="left")
        .with_columns(deg=ur.degree(e))
        .join(third(), on="id", how="inner")
        .collect()
        .to_dicts()
    )
    deg = {r["id"]: r["deg"] for r in e.nodes().with_columns(deg=ur.degree(e)).collect().to_dicts()}
    ref = nested_loop(
        attrs().join(other(), on="id", how="left").collect().to_arrow(),
        third().collect().to_arrow(),
        ["id"],
        "inner",
    )
    assert [(r["id"], r["v"], r["w"], r["x"]) for r in out] == ref
    assert [r["deg"] for r in out] == [deg.get(r["id"]) for r in out]


# --- engine frames on either side -------------------------------------------


def engine_frames():
    e = edges()
    return {
        "graph_column": e.nodes().with_columns(deg=ur.degree(e), pr=ur.pagerank(e)),
        "algorithm": ur.degree(e)._frame(),
        "attr_graph": attrs().with_columns(deg=ur.degree(e, direction="in")),
        "join": attrs().join(other(), on="id", how="left"),
        "hop": ur.hop(e, 2).from_([0, 4]).rename({"src": "id"}),
        "path": ur.shortest_path(e, source=0, target=4).rename({"src": "id"}),
        "walk": ur.random_walk(e, start=[0, 3], steps=3, seed=7).rename({"node": "id"}),
    }


@pytest.mark.parametrize("name", ["graph_column", "algorithm", "attr_graph", "join", "hop"])
@pytest.mark.parametrize("how", HOWS)
def test_engine_frame_on_the_right(name, how):
    right = engine_frames()[name]
    if "v" in right.collect().to_arrow().column_names:
        right = right.rename({"v": "rv"})
    left = ur.from_arrow(pa.table({"id": [4, 0, 2, 9, 2, 1], "z": [1, 2, 3, 4, 5, 6]}), id="id")
    check(left, right, "id", how)


@pytest.mark.parametrize("name", ["graph_column", "algorithm", "attr_graph", "hop", "path", "walk"])
@pytest.mark.parametrize("how", HOWS)
def test_engine_frame_on_the_left(name, how):
    left = engine_frames()[name]
    right = ur.from_arrow(pa.table({"id": [4, 0, 2, 9, 2, 1], "z": [1, 2, 3, 4, 5, 6]}), id="id")
    check(left, right, "id", how)


@pytest.mark.parametrize("how", HOWS)
def test_join_on_a_traversal_result(how):
    e = edges()
    hop = ur.hop(e, 1).from_([0, 2])
    sources = ur.from_arrow(pa.table({"src": [2, 0, 7, 2], "label": ["a", "b", "c", "d"]}), id="src")
    check(hop, sources, "src", how)
    # A traversal on the right, and traversals on both sides.
    check(sources, hop, "src", how)
    path = ur.shortest_path(e, source=0, target=5)
    check(hop, path.select("src", "hop"), "src", how)


def test_join_on_a_traversal_then_group_by():
    e = edges()
    sources = ur.from_arrow(pa.table({"src": [0, 2], "x": [1, 2]}), id="src")
    out = (
        ur.hop(e, 1)
        .from_([0, 2])
        .join(sources, on="src")
        .group_by("x")
        .agg(n=ur.col("src").count())
        .collect()
        .to_dicts()
    )
    hop = ur.hop(e, 1).from_([0, 2]).collect().to_dicts()
    want = {1: sum(r["src"] == 0 for r in hop), 2: sum(r["src"] == 2 for r in hop)}
    assert {r["x"]: r["n"] for r in out} == want


def test_join_after_describe():
    e = edges()
    info = ur.from_arrow(pa.table({"n_nodes": [6, 7], "label": ["six", "seven"]}), id="n_nodes")
    out = ur.describe(e).join(info, on="n_nodes").collect().to_dicts()
    assert [r["label"] for r in out] == ["six"]
