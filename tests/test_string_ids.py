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


# --- every node-valued verb, string ids vs the int-id twin (#53.1) ----------
# The int diamond 0->1,0->2,1->2,2->0 and the string diamond a->b,a->c,b->c,c->a
# are the same graph under 0/1/2 <-> a/b/c (identical first-seen order, so dense
# indices line up). Each verb must return utf8-keyed ids and agree value-for-value
# (or partition-for-partition, for the community labels) with the int twin — a
# gather bug on the string path would otherwise be invisible.
_REMAP = {0: "a", 1: "b", 2: "c"}


def _int_diamond():
    return ur.from_arrow(pa.table({"src": [0, 0, 1, 2], "dst": [1, 2, 2, 0]}), src="src", dst="dst")


def _str_diamond():
    return _sedges(_SRC, _DST)


# Verbs whose per-node value is directly comparable (numeric).
_NUMERIC_VERBS = [
    ("pagerank", lambda e: ur.pagerank(e)),
    ("degree", lambda e: ur.degree(e, direction="both")),
    ("triangle_count", lambda e: ur.triangle_count(e)),
    ("clustering_coefficient", lambda e: ur.clustering_coefficient(e)),
    ("closeness", lambda e: ur.closeness(e)),
    ("betweenness", lambda e: ur.betweenness(e)),
]

# Verbs whose value is an arbitrary community/component label — compare the
# induced partition, not the raw label numbers.
_PARTITION_VERBS = [
    ("connected_components", lambda e: ur.connected_components(e)),
    ("label_propagation", lambda e: ur.label_propagation(e, seed=1)),
    ("louvain", lambda e: ur.louvain(e, seed=1)),
]


def _rows(frame, col):
    df = frame.collect().to_arrow()
    assert df.schema.field("id").type == pa.string() or df.schema.field("id").type == pa.int64()
    return df, {r["id"]: r[col] for r in df.to_pylist()}


def _partition(labels: dict) -> frozenset:
    groups: dict = {}
    for node, label in labels.items():
        groups.setdefault(label, set()).add(node)
    return frozenset(frozenset(g) for g in groups.values())


@pytest.mark.parametrize("col,verb", _NUMERIC_VERBS, ids=[c for c, _ in _NUMERIC_VERBS])
def test_numeric_verb_agrees_int_vs_string(col, verb):
    _, int_rows = _rows(verb(_int_diamond()), col)
    str_df, str_rows = _rows(verb(_str_diamond()), col)
    assert str_df.schema.field("id").type == pa.string()
    assert set(str_rows) == {"a", "b", "c"}
    for i, s in _REMAP.items():
        assert int_rows[i] == pytest.approx(str_rows[s], abs=1e-9)


@pytest.mark.parametrize("col,verb", _PARTITION_VERBS, ids=[c for c, _ in _PARTITION_VERBS])
def test_partition_verb_agrees_int_vs_string(col, verb):
    _, int_rows = _rows(verb(_int_diamond()), col)
    str_df, str_rows = _rows(verb(_str_diamond()), col)
    assert str_df.schema.field("id").type == pa.string()
    assert set(str_rows) == {"a", "b", "c"}
    int_partition = frozenset(frozenset(_REMAP[i] for i in grp) for grp in _partition(int_rows))
    assert int_partition == _partition(str_rows)


def test_weighted_algorithms_over_string_ids():
    # Weighted variants must also key by strings and honor the edge weight column.
    tbl = pa.table({"src": _SRC, "dst": _DST, "w": [1.0, 5.0, 1.0, 1.0]})
    edges = ur.from_arrow(tbl, src="src", dst="dst")
    for verb in (
        lambda e: ur.pagerank(e, weight=ur.col("w")),
        lambda e: ur.closeness(e, weight=ur.col("w")),
        lambda e: ur.betweenness(e, weight=ur.col("w")),
        lambda e: ur.louvain(e, weight=ur.col("w"), seed=1),
    ):
        df = verb(edges).collect().to_arrow()
        assert df.schema.field("id").type == pa.string()
        assert {r["id"] for r in df.to_pylist()} == {"a", "b", "c"}


def test_scan_parquet_string_ids_matches_in_memory(tmp_path):
    # DataFusion reads Parquet strings as string_view; a string-id file must scan
    # and give the same graph as the same table in memory.
    import pyarrow.parquet as pq

    tbl = pa.table({"s": ["a", "b", "c", "a"], "d": ["b", "c", "a", "c"]})
    path = tmp_path / "e.parquet"
    pq.write_table(tbl, path)
    nodes = pa.table({"id": ["a", "b", "c"], "w": [1.0, 2.0, 3.0]})
    npath = tmp_path / "n.parquet"
    pq.write_table(nodes, npath)

    scanned = ur.scan_edges(str(path), src="s", dst="d")
    in_mem = ur.from_arrow(tbl, src="s", dst="d")
    got = ur.pagerank(scanned).collect().to_arrow().sort_by("id")
    want = ur.pagerank(in_mem).collect().to_arrow().sort_by("id")
    assert got.equals(want)
    assert got.column("id").type == pa.string()

    nf = ur.scan_nodes(str(npath), id="id")
    out = nf.with_columns(pr=ur.pagerank(scanned)).collect().to_arrow()
    assert sorted(out.column("id").to_pylist()) == ["a", "b", "c"]


# --- every string layout, scanned and in memory -------------------------------
# Engines hand back strings in more than one Arrow layout: DataFusion/Athena and
# Polars use string_view, pandas categoricals become dictionary-encoded columns,
# and a Parquet file keeps that layout in its embedded Arrow schema (so a scan
# reads it back as written). Every layout must build the same graph as plain
# string ids — through a scan and in memory, as edge ids, node ids and seeds —
# and string attribute columns in those layouts must still work.
_LAYOUTS = {
    "string": lambda v: pa.array(v, pa.string()),
    "large_string": lambda v: pa.array(v, pa.large_string()),
    "string_view": lambda v: pa.array(v, pa.string_view()),
    "dictionary": lambda v: pa.array(v, pa.string()).dictionary_encode(),
}
_E_SRC = ["a", "a", "b", "c", "d", "d"]
_E_DST = ["b", "c", "c", "a", "a", "e"]
_N_IDS = ["a", "b", "c", "d", "e"]
_N_REGION = ["x", "y", "x", "y", "x"]


def _layout_frames(tmp_path, layout, source):
    """The edge/node frames for one string layout, scanned from Parquet or built
    in memory."""
    import pyarrow.parquet as pq

    arr = _LAYOUTS[layout]
    edges = pa.table({"from": arr(_E_SRC), "to": arr(_E_DST), "w": [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]})
    nodes = pa.table({"id": arr(_N_IDS), "region": arr(_N_REGION), "score": [1, 2, 3, 4, 5]})
    if source == "memory":
        return ur.EdgeFrame(edges, src="from", dst="to"), ur.NodeFrame(nodes, id="id")
    epath, npath = tmp_path / f"e_{layout}.parquet", tmp_path / f"n_{layout}.parquet"
    pq.write_table(edges, epath)
    pq.write_table(nodes, npath)
    return (
        ur.scan_edges(str(epath), src="from", dst="to"),
        ur.scan_nodes(str(npath), id="id"),
    )


def _plain(v):
    """A result as plain Python values, so layouts compare equal."""
    if hasattr(v, "collect"):
        v = v.collect()
    if hasattr(v, "to_arrow"):
        v = v.to_arrow()
    return v.to_pylist() if isinstance(v, pa.Table) else v


def _sorted_edges(frame, src="src", dst="dst"):
    tbl = frame.collect().to_arrow()
    return sorted(zip(tbl.column(src).to_pylist(), tbl.column(dst).to_pylist(), strict=True))


_LAYOUT_OPS = {
    "pagerank": lambda e, n: ur.pagerank(e).sort("id"),
    "weighted_pagerank": lambda e, n: ur.pagerank(e, weight=ur.col("w")).sort("id"),
    "degree": lambda e, n: ur.degree(e, direction="both").sort("id"),
    "components": lambda e, n: ur.connected_components(e).sort("id"),
    "triangles": lambda e, n: ur.triangle_count(e).sort("id"),
    "closeness": lambda e, n: ur.closeness(e).sort("id"),
    "louvain": lambda e, n: ur.louvain(e, seed=1).sort("id"),
    "hop": lambda e, n: _sorted_edges(ur.hop(e, n=2).from_(["a"])),
    "shortest_path": lambda e, n: ur.shortest_path(e, "a", "e"),
    "random_walk": lambda e, n: ur.random_walk(e, start=["a", "d"], steps=3, seed=7),
    "density": lambda e, n: ur.density(e),
    "describe": lambda e, n: ur.describe(e),
    "avg_path_length": lambda e, n: ur.avg_path_length(e),
    "edges_nodes": lambda e, n: e.nodes().with_columns(d=ur.degree(e)).sort("id"),
    "filter_then_graph": lambda e, n: ur.pagerank(e.filter(ur.col("from") != "d")).sort("id"),
    "reverse": lambda e, n: ur.pagerank(e.reverse()).sort("id"),
    "edge_collect": lambda e, n: _sorted_edges(e, "from", "to"),
    "node_collect": lambda e, n: [str(r) for r in n.sort("score").collect().to_dicts()],
    "node_filter": lambda e, n: [
        str(r) for r in n.filter(ur.col("region") == "x").sort("id").collect().to_dicts()
    ],
    "with_columns": lambda e, n: (
        n.with_columns(pr=ur.pagerank(e), d=ur.degree(e))
        .sort("id")
        .collect()
        .to_arrow()
        .select(["score", "pr", "d"])
    ),
    "string_attr": lambda e, n: [
        str(v)
        for v in n.filter(ur.col("region") == "x")
        .with_columns(d=ur.degree(e))
        .sort("id")
        .collect()
        .to_arrow()
        .column("region")
        .to_pylist()
    ],
    "neighbors_agg": lambda e, n: (
        n.with_columns(
            m=ur.neighbors(e, direction="in").agg(ur.col("score").mean()),
            r=ur.neighbors(e, direction="in").agg(ur.col("region").n_unique()),
        )
        .sort("id")
        .collect()
        .to_arrow()
        .select(["m", "r"])
    ),
    "seeds_from_nodes": lambda e, n: _sorted_edges(ur.hop(e, n=1).from_(n)),
    "group_by": lambda e, n: [
        (str(r["region"]), r["s"])
        for r in n.with_columns(d=ur.degree(e))
        .group_by("region")
        .agg(s=ur.col("d").sum())
        .sort("s")
        .collect()
        .to_dicts()
    ],
    "join": lambda e, n: (
        e.nodes()
        .with_columns(pr=ur.pagerank(e))
        .join(n, on="id")
        .sort("id")
        .collect()
        .to_arrow()
        .select(["pr", "score"])
    ),
}


@pytest.mark.parametrize("op", list(_LAYOUT_OPS))
@pytest.mark.parametrize("source", ["parquet", "memory"])
@pytest.mark.parametrize("layout", ["large_string", "string_view", "dictionary"])
def test_string_layouts_match_plain_string_ids(tmp_path, layout, source, op):
    run = _LAYOUT_OPS[op]
    want = _plain(run(*_layout_frames(tmp_path, "string", "memory")))
    got = _plain(run(*_layout_frames(tmp_path, layout, source)))
    assert got == want


@pytest.mark.parametrize("layout", list(_LAYOUTS))
def test_string_layout_ids_come_back_as_string(tmp_path, layout):
    # Whatever layout the ids arrived in, result id columns are plain string.
    for source in ("parquet", "memory"):
        edges, _ = _layout_frames(tmp_path, layout, source)
        df = ur.pagerank(edges).collect().to_arrow()
        assert df.schema.field("id").type == pa.string()


def test_string_view_parquet_written_by_an_engine_scans(tmp_path):
    # The reported failure: a Parquet file whose embedded Arrow schema says
    # string_view (as Athena/DataFusion write it) must scan with string ids.
    import pyarrow.parquet as pq

    tbl = pa.table(
        {
            "s": pa.array(["u1", "u2", "u3"], pa.string_view()),
            "d": pa.array(["u2", "u3", "u1"], pa.string_view()),
        }
    )
    path = tmp_path / "views.parquet"
    pq.write_table(tbl, path)
    assert pq.read_schema(path).field("s").type == pa.string_view()
    edges = ur.scan_edges(str(path), src="s", dst="d")
    rows = ur.degree(edges, direction="in").sort("id").collect().to_dicts()
    assert rows == [{"id": f"u{i}", "degree": 1} for i in (1, 2, 3)]


def test_scan_nodes_string_attributes_filter_and_sort(tmp_path):
    # DataFusion read every Parquet string as string_view, so a plain string
    # attribute column of a scanned node file could not be filtered or sorted
    # ("no kernel matching input types (string_view, ...)").
    import pyarrow.parquet as pq

    path = tmp_path / "nodes.parquet"
    pq.write_table(pa.table({"id": [1, 2, 3], "region": ["us", "eu", "us"]}), path)
    nodes = ur.scan_nodes(str(path), id="id")
    out = nodes.filter(ur.col("region") == "us").sort("region").collect().to_arrow()
    assert out.schema.field("region").type == pa.string()
    assert out.column("id").to_pylist() == [1, 3]


def test_polars_arrow_stream_string_ids():
    # polars' Arrow C stream exports strings as string_view.
    pl = pytest.importorskip("polars")
    df = pl.DataFrame({"s": ["a", "b", "c"], "d": ["b", "c", "a"], "tag": ["x", "y", "x"]})
    tbl = pa.table(df)
    assert tbl.schema.field("s").type == pa.string_view()
    edges = ur.EdgeFrame(tbl, src="s", dst="d")
    rows = ur.degree(edges.filter(ur.col("tag") == "x")).sort("id").collect().to_dicts()
    assert rows == [{"id": "a", "degree": 1}, {"id": "b", "degree": 0}, {"id": "c", "degree": 1}]


def test_dictionary_ids_with_integer_values_canonicalize_to_int64():
    tbl = pa.table(
        {
            "s": pa.array([10, 20, 10], pa.int32()).dictionary_encode(),
            "d": pa.array([20, 30, 30], pa.int32()).dictionary_encode(),
        }
    )
    df = ur.degree(ur.EdgeFrame(tbl, src="s", dst="d")).sort("id").collect().to_arrow()
    assert df.schema.field("id").type == pa.int64()
    assert df.column("id").to_pylist() == [10, 20, 30]


def test_dictionary_of_unsupported_values_is_rejected():
    tbl = pa.table(
        {
            "s": pa.array([1.0, 2.0]).dictionary_encode(),
            "d": pa.array([2.0, 1.0]).dictionary_encode(),
        }
    )
    with pytest.raises(TypeError, match="integer or string"):
        ur.EdgeFrame(tbl, src="s", dst="d")
