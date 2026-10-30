"""One float semantics for keys (#215): ``group_by`` keys, ``distinct``,
``n_unique`` and join keys compare floats in the total order filters and
``sort`` use. ``-0.0`` equals ``0.0``, every NaN equals every NaN (any sign or
payload), NaN joins NaN, and nulls never match in a join but form one group.
A group or distinct row shows its first row's value.

Each case runs over an in-memory frame, the same rows scanned from Parquet (with
small row groups), and an engine frame (after a graph column), against a
reference written here in Python.
"""

from __future__ import annotations

import math
import struct

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa import col

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _f64(bits: int) -> float:
    return struct.unpack("<d", struct.pack("<Q", bits))[0]


def _bits(v: float | None) -> int | None:
    return None if v is None else struct.unpack("<Q", struct.pack("<d", v))[0]


NEG_NAN = _f64(0xFFF8_0000_0000_0000)
POOL = [
    -0.0,
    1.5,
    0.0,
    math.nan,
    _f64(0xFFF8_0000_0000_0001),
    None,
    _f64(0x7FF8_DEAD_BEEF_0000),
    math.inf,
    -math.inf,
    NEG_NAN,
    -1.5,
    _f64(0x7FF0_0000_0000_0005),
]
N = 600


def _class(v: float | None) -> object:
    if v is None:
        return None
    if math.isnan(v):
        return "nan"
    return v + 0.0


def _table(n: int = N) -> pa.Table:
    return pa.table(
        {
            "id": list(range(n)),
            "k": [POOL[(i * 7 + i // 13) % len(POOL)] for i in range(n)],
            "s": [f"s{(i * 5) % 3}" for i in range(n)],
            "w": [POOL[(i * 11 + 3) % len(POOL)] for i in range(n)],
            "v": [i % 17 for i in range(n)],
        }
    )


def _frames(t: pa.Table, tmp_path) -> dict[str, ur.NodeFrame]:
    path = tmp_path / "rows.parquet"
    pq.write_table(t, path, row_group_size=97)
    ring = pa.table(
        {"s": list(range(t.num_rows)), "d": [(i + 1) % t.num_rows for i in range(t.num_rows)]}
    )
    edges = ur.from_arrow(ring, src="s", dst="d")
    return {
        "memory": ur.from_arrow(t, id="id"),
        "parquet": ur.scan_nodes(str(path), id="id"),
        "engine": ur.from_arrow(t, id="id").with_columns(dg=ur.degree(edges)),
    }


def _first_rows(t: pa.Table, keys: list[str]) -> list[int]:
    seen: dict[tuple, int] = {}
    cols = {k: t.column(k).to_pylist() for k in keys}
    for i in range(t.num_rows):
        seen.setdefault(tuple(_class(cols[k][i]) if k != "s" else cols[k][i] for k in keys), i)
    return list(seen.values())


@pytest.mark.parametrize("keys", [["k"], ["k", "s"], ["s", "k", "w"]])
def test_group_by_float_keys_in_total_order(tmp_path, keys):
    t = _table()
    firsts = _first_rows(t, keys)
    v = t.column("v").to_pylist()
    w = t.column("w").to_pylist()
    for name, frame in _frames(t, tmp_path).items():
        out = frame.group_by(*keys).agg(n=col("v").count(), t=col("v").sum(), u=col("w").n_unique())
        got = out.collect().to_arrow()
        assert got.num_rows == len(firsts), name
        for key in keys:
            want = [t.column(key)[i].as_py() for i in firsts]
            if key == "s":
                assert got.column(key).to_pylist() == want, name
            else:
                assert [_bits(x) for x in got.column(key).to_pylist()] == [
                    _bits(x) for x in want
                ], name
        # count, sum and n_unique per group, from the reference.
        groups: dict[tuple, list[int]] = {}
        for i in range(t.num_rows):
            gid = tuple(
                _class(t.column(c)[i].as_py()) if c != "s" else t.column(c)[i].as_py() for c in keys
            )
            groups.setdefault(gid, []).append(i)
        rows = list(groups.values())
        assert got.column("n").to_pylist() == [len(r) for r in rows], name
        assert got.column("t").to_pylist() == [sum(v[i] for i in r) for r in rows], name
        want_u = [len({_class(w[i]) for i in r if w[i] is not None}) for r in rows]
        assert got.column("u").to_pylist() == want_u, name


def test_group_by_shows_the_first_appearance_value():
    t = pa.table({"k": [-0.0, 0.0, NEG_NAN, math.nan, None, 0.0, None], "v": [1, 2, 3, 4, 5, 6, 7]})
    out = ur.from_arrow(t.append_column("id", pa.array(range(7))), id="id")
    got = out.group_by("k").agg(s=col("v").sum()).collect().to_arrow()
    assert [_bits(x) for x in got.column("k").to_pylist()] == [_bits(-0.0), _bits(NEG_NAN), None]
    assert got.column("s").to_pylist() == [9, 7, 12]
    t2 = pa.table({"id": [0, 1, 2], "k": [0.0, -0.0, math.nan]})
    got = ur.from_arrow(t2, id="id").group_by("k").agg(n=col("k").count()).collect().to_arrow()
    assert [_bits(x) for x in got.column("k").to_pylist()] == [_bits(0.0), _bits(math.nan)]


def test_a_float_aggregate_over_a_key_column_folds_the_original_values():
    # The key groups canonical; min/max over the same column fold the original
    # values, so a group of -0.0 keys has min and max -0.0 (canonical values
    # would give 0.0). A sum starts at +0.0, by pyarrow's rule. (No ties between
    # -0.0 and 0.0 here: which one pyarrow's min/max returns differs by platform.)
    k = [-0.0, -0.0, NEG_NAN, math.nan, 2.5]
    t = pa.table({"id": list(range(5)), "k": k})
    aggs = {"s": col("k").sum(), "m": col("k").min(), "x": col("k").max()}
    got = ur.from_arrow(t, id="id").group_by("k").agg(**aggs).collect().to_arrow()
    assert [_bits(v) for v in got.column("k").to_pylist()] == [
        _bits(-0.0),
        _bits(NEG_NAN),
        _bits(2.5),
    ]
    assert _bits(got.column("m")[0].as_py()) == _bits(-0.0)
    assert _bits(got.column("x")[0].as_py()) == _bits(-0.0)
    assert got.column("s").to_pylist()[0] == 0.0
    assert all(math.isnan(got.column(c)[1].as_py()) for c in ("s", "m", "x"))
    assert [got.column(c)[2].as_py() for c in ("s", "m", "x")] == [2.5, 2.5, 2.5]


def test_distinct_compares_every_float_column_in_total_order(tmp_path):
    t = _table()
    cols = ["k", "s", "w"]
    firsts = _first_rows(t, cols)
    for name, frame in _frames(t, tmp_path).items():
        got = frame.select(*cols).distinct().collect().to_arrow()
        assert got.num_rows == len(firsts), name
        for c in cols:
            want = [t.column(c)[i].as_py() for i in firsts]
            if c == "s":
                assert got.column(c).to_pylist() == want
            else:
                assert [_bits(x) for x in got.column(c).to_pylist()] == [_bits(x) for x in want], (
                    name
                )


@pytest.mark.parametrize("value_type", [pa.float64(), pa.float32()])
def test_dictionary_float_keys(value_type):
    vals = [-0.0, 0.0, math.nan, NEG_NAN, None, 2.0, -0.0]
    k = pa.array(vals, value_type).dictionary_encode()
    t = pa.table({"id": list(range(7)), "k": k, "v": list(range(7))})
    frame = ur.from_arrow(t, id="id")
    got = frame.group_by("k").agg(n=col("v").count()).collect().to_arrow()
    assert pa.types.is_dictionary(got.schema.field("k").type)
    keys = got.column("k").cast(value_type).to_pylist()
    assert [None if x is None else (math.isnan(x), math.copysign(1, x)) for x in keys] == [
        (False, -1.0),
        (True, 1.0),
        None,
        (False, 1.0),
    ]
    assert got.column("n").to_pylist() == [3, 2, 1, 1]
    assert frame.select("k").distinct().collect().to_arrow().num_rows == 4
    t = t.append_column("g", pa.array([1] * 7))
    nu = ur.from_arrow(t, id="id").group_by("g").agg(u=col("k").n_unique()).collect()
    assert nu.to_dicts() == [{"g": 1, "u": 3}]


def test_n_unique_counts_classes():
    w = [-0.0, 0.0, math.nan, NEG_NAN, _f64(0x7FF8_0000_0000_0042), None, math.inf, -math.inf, 1.0]
    t = pa.table({"id": list(range(9)), "g": [1] * 9, "w": w})
    out = ur.from_arrow(t, id="id").group_by("g").agg(u=col("w").n_unique()).collect()
    assert out.to_dicts() == [{"g": 1, "u": 5}]
    t32 = t.set_column(2, "w", pa.array(w, pa.float32()))
    out = ur.from_arrow(t32, id="id").group_by("g").agg(u=col("w").n_unique()).collect()
    assert out.to_dicts() == [{"g": 1, "u": 5}]


HOWS = ["inner", "left", "right", "outer", "semi", "anti"]


def _join_reference(left: pa.Table, right: pa.Table, how: str) -> list[tuple]:
    lk, la = left.column("k").to_pylist(), left.column("a").to_pylist()
    rk, rb = right.column("k").to_pylist(), right.column("b").to_pylist()

    def eq(x, y):
        return x is not None and y is not None and _class(x) == _class(y)

    out = []
    if how == "right":
        for k, b in zip(rk, rb, strict=True):
            m = [i for i in range(len(lk)) if eq(lk[i], k)]
            if not m:
                out.append((_bits(k), None, b))
            out.extend((_bits(lk[i]), la[i], b) for i in m)
        return out
    for k, a in zip(lk, la, strict=True):
        m = [j for j in range(len(rk)) if eq(k, rk[j])]
        if how in ("semi", "anti"):
            if bool(m) == (how == "semi"):
                out.append((_bits(k), a))
            continue
        if not m and how != "inner":
            out.append((_bits(k), a, None))
        out.extend((_bits(k), a, rb[j]) for j in m)
    if how == "outer":
        for k, b in zip(rk, rb, strict=True):
            if not any(eq(x, k) for x in lk):
                out.append((_bits(k), None, b))
    return out


@pytest.mark.parametrize("how", HOWS)
def test_float_join_keys_in_total_order(tmp_path, how):
    left = pa.table({"k": [POOL[(i * 5) % len(POOL)] for i in range(40)], "a": list(range(40))})
    right = pa.table(
        {"k": [POOL[(i * 7 + 2) % len(POOL)] for i in range(30)], "b": list(range(100, 130))}
    )
    want = _join_reference(left, right, how)
    lpath, rpath = tmp_path / "l.parquet", tmp_path / "r.parquet"
    pq.write_table(left, lpath, row_group_size=7)
    pq.write_table(right, rpath, row_group_size=7)
    ring = pa.table({"s": list(range(40)), "d": [(i + 1) % 40 for i in range(40)]})
    edges = ur.from_arrow(ring, src="s", dst="d")
    lefts = {
        "memory": ur.from_arrow(left, id="a"),
        "parquet": ur.scan_nodes(str(lpath), id="a"),
        "engine": ur.from_arrow(left, id="a").with_columns(dg=ur.degree(edges)).select("k", "a"),
    }
    rights = {"memory": ur.from_arrow(right, id="b"), "parquet": ur.scan_nodes(str(rpath), id="b")}
    for ln, lf in lefts.items():
        for rn, rf in rights.items():
            got = lf.join(rf, on="k", how=how).collect().to_arrow()
            cols = ["k", "a"] if how in ("semi", "anti") else ["k", "a", "b"]
            rows = list(zip(*[got.column(c).to_pylist() for c in cols], strict=True))
            rows = [(_bits(r[0]), *r[1:]) for r in rows]
            assert rows == want, (ln, rn)


def test_multi_key_join_with_a_float_key():
    left = pa.table({"k": [-0.0, NEG_NAN, 1.0, None], "s": ["x", "y", "x", "x"], "a": [1, 2, 3, 4]})
    right = pa.table(
        {"k": [0.0, math.nan, 1.0, None], "s": ["x", "y", "y", "x"], "b": [5, 6, 7, 8]}
    )
    got = ur.from_arrow(left, id="a").join(ur.from_arrow(right, id="b"), on=["k", "s"]).collect()
    assert [(r["a"], r["b"]) for r in got.to_dicts()] == [(1, 5), (2, 6)]


def test_an_integer_key_joins_a_float_key_with_zero_and_negative_zero():
    left = pa.table({"k": [0, 1, None], "a": [1, 2, 3]})
    right = pa.table({"k": [-0.0, 0.0, math.nan, 1.0], "b": [4, 5, 6, 7]})
    got = ur.from_arrow(left, id="a").join(ur.from_arrow(right, id="b"), on="k").collect()
    assert [(r["a"], r["b"]) for r in got.to_dicts()] == [(1, 4), (1, 5), (2, 7)]


@pytest.mark.parametrize("how", HOWS)
def test_float32_and_float64_keys_join_in_total_order(how):
    left = pa.table({"k": pa.array([-0.0, NEG_NAN, 2.5, None], pa.float32()), "a": [1, 2, 3, 4]})
    right = pa.table({"k": pa.array([0.0, math.nan, 7.0], pa.float64()), "b": [5, 6, 7]})
    got = ur.from_arrow(left, id="a").join(ur.from_arrow(right, id="b"), on="k", how=how).collect()
    out = got.to_arrow()
    assert out.schema.field("k").type == pa.float32()
    pairs = [(r["a"], r.get("b")) for r in got.to_dicts()]
    want = {
        "inner": [(1, 5), (2, 6)],
        "left": [(1, 5), (2, 6), (3, None), (4, None)],
        "right": [(1, 5), (2, 6), (None, 7)],
        "outer": [(1, 5), (2, 6), (3, None), (4, None), (None, 7)],
        "semi": [(1, None), (2, None)],
        "anti": [(3, None), (4, None)],
    }[how]
    assert pairs == want
