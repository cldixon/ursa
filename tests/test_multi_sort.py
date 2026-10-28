"""Multi-key ``sort`` (issue #163).

``sort(by, *more_by, descending=...)`` orders rows lexicographically by the keys
in order (later keys break ties in earlier ones). ``descending`` is one bool for
every key or a list with one bool per key. Nulls sort last for every key in either
direction, the sort is stable, and dictionary-encoded keys order by value. Every
kind of frame is covered: in-memory and scanned node/edge frames, grouped and
join+group results, graph-derived frames and traversal outputs. Expected values
are hard-coded and cross-checked against pyarrow's own multi-key ``sort_by``.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# Nulls in both keys; ids 2 and 7 tie on (c, v), so stability decides their order.
T = pa.table(
    {
        "id": [1, 2, 3, 4, 5, 6, 7, 8],
        "c": ["x", "y", "x", None, "y", "x", "y", None],
        "v": [3, 1, 2, 5, None, None, 1, 0],
    }
)


def _ids(frame) -> list:
    return frame.collect().to_arrow().column("id").to_pylist()


def _pa_ids(table: pa.Table, keys: list[tuple[str, str]]) -> list:
    # pyarrow's sort_by is stable with nulls at the end — the reference ordering.
    return table.sort_by(keys).column("id").to_pylist()


def _frames(tmp_path, table: pa.Table) -> list[ur.NodeFrame]:
    path = tmp_path / "t.parquet"
    pq.write_table(table, path)
    return [ur.NodeFrame(table, id="id"), ur.scan_nodes(str(path), id="id")]


CASES = [
    # (by, descending, expected ids)
    (["c", "v"], False, [3, 1, 6, 2, 7, 5, 8, 4]),
    (["c", "v"], [False, True], [1, 3, 6, 2, 7, 5, 4, 8]),
    (["c", "v"], [True, False], [2, 7, 5, 3, 1, 6, 8, 4]),
    (["c", "v"], True, [2, 7, 5, 1, 3, 6, 4, 8]),
    (["v", "c"], False, [8, 2, 7, 3, 1, 4, 6, 5]),
    (["c", "v", "id"], [False, False, True], [3, 1, 6, 7, 2, 5, 8, 4]),
]


@pytest.mark.parametrize(("by", "descending", "expected"), CASES)
def test_multi_key_sort_matches_pyarrow(tmp_path, by, descending, expected):
    flags = descending if isinstance(descending, list) else [descending] * len(by)
    ref = _pa_ids(
        T, [(k, "descending" if d else "ascending") for k, d in zip(by, flags, strict=True)]
    )
    assert ref == expected
    for nf in _frames(tmp_path, T):
        assert _ids(nf.sort(by, descending=descending)) == expected


def test_ties_broken_by_second_key_and_stable_on_full_ties():
    # Sorting by c alone keeps input order within each c (stable); adding v as a
    # second key reorders inside each c group; ids 2/7 tie on both and stay 2, 7.
    nf = ur.NodeFrame(T, id="id")
    assert _ids(nf.sort("c")) == [1, 3, 6, 2, 5, 7, 4, 8]
    assert _ids(nf.sort(["c", "v"])) == [3, 1, 6, 2, 7, 5, 8, 4]


def test_more_by_varargs_and_col_exprs_equal_the_list_form():
    nf = ur.NodeFrame(T, id="id")
    expected = [1, 3, 6, 2, 7, 5, 4, 8]
    assert _ids(nf.sort("c", "v", descending=[False, True])) == expected
    assert _ids(nf.sort(ur.col("c"), ur.col("v"), descending=[False, True])) == expected
    assert _ids(nf.sort([ur.col("c"), "v"], descending=(False, True))) == expected


def test_single_key_list_and_string_agree():
    nf = ur.NodeFrame(T, id="id")
    assert _ids(nf.sort(["v"], descending=[True])) == _ids(nf.sort("v", descending=True))
    assert _ids(nf.sort("v", descending=True)) == [4, 1, 3, 2, 7, 8, 5, 6]


def test_dictionary_encoded_keys_sort_by_value(tmp_path):
    # Dictionaries in reverse value order, so an index-order sort would be wrong.
    c = pa.DictionaryArray.from_arrays(
        pa.array([1, 0, 1, None, 0, 1, 0, None], pa.int32()), pa.array(["y", "x"])
    )
    v = pa.DictionaryArray.from_arrays(
        pa.array([1, 3, 2, 0, None, None, 3, 4], pa.int32()), pa.array([5, 3, 2, 1, 0])
    )
    table = pa.table({"id": T.column("id"), "c": c, "v": v})
    assert table.column("v").to_pylist() == T.column("v").to_pylist()
    for nf in _frames(tmp_path, table):
        out = nf.sort(["c", "v"], descending=[False, True]).collect().to_arrow()
        assert out.column("id").to_pylist() == [1, 3, 6, 2, 7, 5, 4, 8]
        assert pa.types.is_dictionary(out.schema.field("c").type)
        assert pa.types.is_dictionary(out.schema.field("v").type)


def test_edge_frame_multi_key_sort(tmp_path):
    edges = pa.table(
        {
            "s": [1, 2, 1, 2, 1, 3],
            "d": [2, 3, 3, 1, 4, 1],
            "w": [0.5, 0.5, 0.5, None, 0.1, 0.9],
        }
    )
    path = tmp_path / "e.parquet"
    pq.write_table(edges, path)
    for ef in [ur.from_arrow(edges, src="s", dst="d"), ur.scan_edges(str(path), src="s", dst="d")]:
        out = ef.sort(["w", "s", "d"], descending=[True, False, True]).collect().to_arrow()
        pairs = list(zip(out.column("s").to_pylist(), out.column("d").to_pylist(), strict=True))
        assert pairs == [(3, 1), (1, 3), (1, 2), (2, 3), (1, 4), (2, 1)]
        ref = edges.sort_by([("w", "descending"), ("s", "ascending"), ("d", "descending")])
        assert out.to_pylist() == ref.to_pylist()


def test_grouped_result_multi_key_sort():
    nodes = pa.table(
        {
            "id": [1, 2, 3, 4, 5, 6],
            "region": ["us", "eu", "us", "ap", "eu", "ap"],
            "tier": [1, 1, 2, 1, 1, 2],
            "v": [10, 20, 30, 40, 50, 60],
        }
    )
    out = (
        ur.from_arrow(nodes, id="id")
        .group_by("region", "tier")
        .agg(n=ur.col("v").count(), total=ur.col("v").sum())
        .sort(["n", "region", "tier"], descending=[True, False, True])
        .collect()
        .to_arrow()
    )
    assert out.select(["region", "tier", "n", "total"]).to_pylist() == [
        {"region": "eu", "tier": 1, "n": 2, "total": 70},
        {"region": "ap", "tier": 2, "n": 1, "total": 60},
        {"region": "ap", "tier": 1, "n": 1, "total": 40},
        {"region": "us", "tier": 2, "n": 1, "total": 30},
        {"region": "us", "tier": 1, "n": 1, "total": 10},
    ]


def test_join_then_group_by_multi_key_sort():
    sales = pa.table({"id": [0, 1, 2, 3, 4, 5], "v": [10, 20, 30, 40, 50, 60]})
    regions = pa.table({"id": [0, 1, 2, 3, 4, 5], "region": ["us", "us", "eu", "eu", "ap", "ap"]})
    out = (
        ur.from_arrow(sales, id="id")
        .join(ur.from_arrow(regions, id="id"), on="id")
        .group_by("region")
        .agg(n=ur.col("v").count(), total=ur.col("v").sum())
        .sort(["n", "region"], descending=[True, False])
        .collect()
        .to_arrow()
    )
    # Every group has n=2, so region (ascending) decides.
    assert out.column("region").to_pylist() == ["ap", "eu", "us"]
    assert out.column("total").to_pylist() == [110, 70, 30]


# --- graph-derived frames and traversals -------------------------------------

# 1->2, 1->3, 2->3, 3->1, 4->1, 4->2: out-degree 1:2, 2:1, 3:1, 4:2.
EDGES = pa.table({"src": [1, 1, 2, 3, 4, 4], "dst": [2, 3, 3, 1, 1, 2]})
CLUBS = pa.table({"id": [1, 2, 3, 4], "club": ["b", "a", "b", "a"]})


def test_graph_derived_frame_multi_key_sort():
    edges = ur.from_arrow(EDGES, src="src", dst="dst")
    out = (
        edges.nodes()
        .with_columns(deg=ur.degree(edges, direction="out"))
        .sort(["deg", "id"], descending=[True, False])
        .collect()
        .to_arrow()
    )
    assert out.column("id").to_pylist() == [1, 4, 2, 3]
    assert out.column("deg").to_pylist() == [2, 2, 1, 1]
    out = (
        edges.nodes()
        .with_columns(deg=ur.degree(edges, direction="out"))
        .sort("deg", "id", descending=True)
        .collect()
        .to_arrow()
    )
    assert out.column("id").to_pylist() == [4, 1, 3, 2]


def test_graph_derived_frame_with_attributes_mixed_directions():
    edges = ur.from_arrow(EDGES, src="src", dst="dst")
    out = (
        ur.from_arrow(CLUBS, id="id")
        .with_columns(deg=ur.degree(edges, direction="out"), pagerank=ur.pagerank(edges))
        .sort(["club", "pagerank"], descending=[False, True])
        .collect()
        .to_arrow()
    )
    pr = dict(zip(out.column("id").to_pylist(), out.column("pagerank").to_pylist(), strict=True))
    expected = sorted(CLUBS.to_pylist(), key=lambda r: (r["club"], -pr[r["id"]]))
    assert out.column("id").to_pylist() == [r["id"] for r in expected]
    assert out.column("club").to_pylist() == ["a", "a", "b", "b"]
    # Three keys, mixed directions: club desc, deg asc, id desc.
    out = (
        ur.from_arrow(CLUBS, id="id")
        .with_columns(deg=ur.degree(edges, direction="out"))
        .sort(["club", "deg", "id"], descending=[True, False, True])
        .collect()
        .to_arrow()
    )
    assert out.column("id").to_pylist() == [3, 1, 2, 4]


def test_graph_derived_frame_nulls_last_in_both_keys():
    # Node 5 has no club and node 6 has no tier; nulls go last for every key and
    # either direction, as on a plain frame.
    edges = ur.from_arrow(
        pa.table({"src": [1, 2, 3, 4, 5, 6], "dst": [2, 3, 4, 5, 6, 1]}), src="src", dst="dst"
    )
    attrs = pa.table(
        {
            "id": [1, 2, 3, 4, 5, 6],
            "club": ["a", "b", "a", "b", None, "a"],
            "tier": [2, 1, 1, None, 1, None],
        }
    )
    frame = ur.from_arrow(attrs, id="id").with_columns(deg=ur.degree(edges, direction="out"))
    asc = _ids(frame.sort(["club", "tier"]))
    desc = _ids(frame.sort(["club", "tier"], descending=True))
    assert asc == [3, 1, 6, 2, 4, 5]
    assert desc == [2, 4, 1, 3, 6, 5]
    assert asc == _pa_ids(attrs, [("club", "ascending"), ("tier", "ascending")])
    assert desc == _pa_ids(attrs, [("club", "descending"), ("tier", "descending")])
    # The plain (pyarrow-tail) frame orders identically.
    plain = ur.from_arrow(attrs, id="id")
    assert _ids(plain.sort(["club", "tier"])) == asc
    assert _ids(plain.sort(["club", "tier"], descending=True)) == desc


def test_hop_multi_key_sort():
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 1], "d": [0, 0, 0, 4]}), src="s", dst="d")
    out = (
        ur.hop(edges, n=1)
        .from_([1, 2, 3])
        .sort(["dst", "src"], descending=[False, True])
        .collect()
        .to_arrow()
    )
    pairs = list(zip(out.column("src").to_pylist(), out.column("dst").to_pylist(), strict=True))
    assert pairs == [(3, 0), (2, 0), (1, 0), (1, 4)]


def test_random_walk_multi_key_sort():
    line = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    out = (
        ur.random_walk(line, start=[0, 1], steps=2, seed=3)
        .sort(["step", "walk_id"], descending=[True, False])
        .collect()
        .to_arrow()
    )
    keys = list(zip(out.column("step").to_pylist(), out.column("walk_id").to_pylist(), strict=True))
    assert keys == sorted(keys, key=lambda k: (-k[0], k[1]))
    assert out.num_rows == 6


def test_shortest_path_multi_key_sort():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    out = ur.shortest_path(edges, 0, 3).sort(["dst", "src"], descending=True).collect().to_arrow()
    assert out.column("src").to_pylist() == [2, 1, 0]


# --- errors -----------------------------------------------------------------


def test_errors_plain_frame():
    nf = ur.NodeFrame(T, id="id")
    with pytest.raises(ValueError, match="at least one column"):
        nf.sort([]).collect()
    with pytest.raises(ValueError, match="2 descending flag"):
        nf.sort("c", descending=[True, False]).collect()
    with pytest.raises(ValueError, match="1 descending flag"):
        nf.sort(["c", "v"], descending=[True]).collect()
    with pytest.raises(ValueError, match="unknown column 'nope'"):
        nf.sort(["c", "nope"]).collect()
    with pytest.raises(ValueError, match=r"column names, ur\.col"):
        nf.sort([3]).collect()


def test_errors_native_engine():
    edges = ur.from_arrow(EDGES, src="src", dst="dst")
    frame = edges.nodes().with_columns(deg=ur.degree(edges))
    with pytest.raises(ValueError, match="1 descending flag"):
        frame.sort(["deg", "id"], descending=[True]).collect()
    with pytest.raises(ValueError, match="at least one column"):
        frame.sort([]).collect()
    with pytest.raises(ValueError, match="unknown column 'nope'"):
        frame.sort(["deg", "nope"]).collect()
