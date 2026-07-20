"""String node ids end to end (Python → PyO3 → ursa-core).

Node ids may be int64 (the fast path) or string (covering UUID-as-string),
auto-detected from the Arrow column type. These tests drive the whole surface —
algorithms, enrichment, traversals, walks, neighbour aggregation — with string
ids, and check that results come back keyed by the original strings. A mixed
test confirms an int-id graph and the equivalent string-id graph agree. Skipped
if the native extension has not been built.
"""

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _sedges(src, dst):
    return ur.from_arrow(pa.table({"src": src, "dst": dst}), src="src", dst="dst")


# a -> b, a -> c, b -> c, c -> a  (a directed triangle-with-chord over string ids)
_SRC = ["a", "a", "b", "c"]
_DST = ["b", "c", "c", "a"]


def test_pagerank_over_string_ids_is_keyed_by_strings():
    edges = _sedges(_SRC, _DST)
    df = ur.pagerank(edges).collect().to_arrow()
    assert df.schema.field("id").type == pa.string()
    rows = {r["id"]: r["pagerank"] for r in df.to_pylist()}
    assert set(rows) == {"a", "b", "c"}
    assert abs(sum(rows.values()) - 1.0) < 1e-6


def test_degree_over_string_ids():
    edges = _sedges(_SRC, _DST)
    out = {r["id"]: r["degree"] for r in ur.degree(edges, direction="in").collect().to_dicts()}
    # in-degrees: a<-c (1), b<-a (1), c<-a,b (2)
    assert out == {"a": 1, "b": 1, "c": 2}


def test_enrichment_joins_by_string_id():
    edges = _sedges(_SRC, _DST)
    nodes = ur.from_arrow(
        pa.table({"id": ["a", "b", "c"], "team": ["x", "x", "y"], "score": [5, 2, 4]}),
        id="id",
    )
    df = (
        nodes.with_columns(
            pr=ur.pagerank(edges),
            nbr=ur.neighbors(edges, direction="in").agg(ur.col("score").mean()),
        )
        .filter(ur.col("score") > 1)
        .collect()
        .to_arrow()
    )
    assert df.schema.field("id").type == pa.string()
    rows = {r["id"]: r for r in df.to_pylist()}
    assert set(rows) == {"a", "b", "c"}
    # c's in-neighbours are a (5) and b (2) -> mean 3.5
    assert abs(rows["c"]["nbr"] - 3.5) < 1e-9


def test_hop_over_string_ids_returns_string_edges():
    edges = _sedges(_SRC, _DST)
    reached = ur.hop(edges, n=1).from_(["a"]).sort("dst").collect().to_arrow()
    assert reached.schema.field("src").type == pa.string()
    assert reached.schema.field("dst").type == pa.string()
    pairs = {(r["src"], r["dst"]) for r in reached.to_pylist()}
    assert pairs == {("a", "b"), ("a", "c")}


def test_shortest_path_over_string_ids():
    # a -> b -> c -> d line
    edges = _sedges(["a", "b", "c"], ["b", "c", "d"])
    path = ur.shortest_path(edges, "a", "d").collect().to_arrow()
    assert path.schema.field("src").type == pa.string()
    rows = path.to_pylist()
    assert [r["src"] for r in rows] == ["a", "b", "c"]
    assert [r["dst"] for r in rows] == ["b", "c", "d"]


def test_random_walk_over_string_ids():
    edges = _sedges(["a", "b", "c"], ["b", "c", "d"])
    walk = ur.random_walk(edges, start=["a"], steps=3, seed=7).collect().to_arrow()
    assert walk.schema.field("node").type == pa.string()
    # forced line a->b->c->d
    assert [r["node"] for r in walk.to_pylist()] == ["a", "b", "c", "d"]


def test_string_and_int_graphs_agree_on_pagerank_ranking():
    # Same logical graph, int ids 0/1/2 vs string ids a/b/c.
    int_edges = ur.from_arrow(
        pa.table({"src": [0, 0, 1, 2], "dst": [1, 2, 2, 0]}), src="src", dst="dst"
    )
    str_edges = _sedges(_SRC, _DST)
    int_rows = {r["id"]: r["pagerank"] for r in ur.pagerank(int_edges).collect().to_dicts()}
    str_rows = {r["id"]: r["pagerank"] for r in ur.pagerank(str_edges).collect().to_dicts()}
    remap = {0: "a", 1: "b", 2: "c"}
    for i, s in remap.items():
        assert abs(int_rows[i] - str_rows[s]) < 1e-9


def test_mixed_src_dst_id_types_error():
    # src int, dst string -> a clear error (not a silent mismatch).
    edges = ur.from_arrow(
        pa.table({"src": pa.array([1, 2]), "dst": pa.array(["a", "b"])}), src="src", dst="dst"
    )
    with pytest.raises(ValueError, match="same type"):
        ur.pagerank(edges).collect()
