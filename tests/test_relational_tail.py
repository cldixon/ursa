"""rename / sample / distinct — the shared relational tail (issues #18, #67).

These verbs run on top of any graph/traversal/plain output via the one shared
tail (`apply_tail` in Rust; `_apply_pyarrow_tail` for plain frames). rename
relabels output columns; sample takes a deterministic seeded subset; distinct
(newly available on node queries) collapses duplicate rows.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

EDGES = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")


def _nodes_file(tmp_path) -> str:
    path = tmp_path / "nodes.csv"
    path.write_text("id,region,capacity\n0,us,10\n1,us,20\n2,eu,30\n3,eu,40\n")
    return str(path)


# --- rename ----------------------------------------------------------------


def test_rename_node_query():
    out = EDGES.nodes().with_columns(pr=ur.pagerank(EDGES)).rename({"pr": "score"}).collect()
    cols = out.to_arrow().column_names
    assert "score" in cols and "pr" not in cols


def test_rename_plain_node_frame(tmp_path):
    out = ur.scan_nodes(_nodes_file(tmp_path), id="id").rename({"region": "r"}).collect()
    cols = out.to_arrow().column_names
    assert "r" in cols and "region" not in cols


def test_rename_traversal_edge_frame():
    out = ur.hop(EDGES, 1).from_([1, 2, 3]).rename({"dst": "to"}).collect()
    cols = out.to_arrow().column_names
    assert "to" in cols and "dst" not in cols


def test_rename_unknown_column_raises():
    with pytest.raises(Exception, match="rename"):
        EDGES.nodes().with_columns(pr=ur.pagerank(EDGES)).rename({"nope": "x"}).collect()


# --- sample ----------------------------------------------------------------


def _rows(df) -> list:
    t = df.to_arrow()
    return list(zip(*[t.column(c).to_pylist() for c in t.column_names], strict=True))


def test_sample_is_seed_deterministic():
    base = EDGES.nodes().with_columns(pr=ur.pagerank(EDGES))
    a = _rows(base.sample(2, seed=7).collect())
    b = _rows(base.sample(2, seed=7).collect())
    assert a == b
    assert len(a) == 2


def test_sample_seed_none_is_reproducible():
    base = EDGES.nodes().with_columns(pr=ur.pagerank(EDGES))
    assert _rows(base.sample(2).collect()) == _rows(base.sample(2).collect())


def test_sample_clamps_when_n_exceeds_rows():
    out = EDGES.nodes().with_columns(pr=ur.pagerank(EDGES)).sample(1000, seed=1).collect()
    assert out.to_arrow().num_rows == 4  # only 4 nodes


def test_sample_without_replacement_is_a_subset():
    full = EDGES.nodes().with_columns(pr=ur.pagerank(EDGES)).collect()
    sampled = EDGES.nodes().with_columns(pr=ur.pagerank(EDGES)).sample(2, seed=3).collect()
    full_rows = set(_rows(full))
    srows = _rows(sampled)
    assert len(srows) == len(set(srows)) == 2  # distinct positions
    assert set(srows) <= full_rows


def test_sample_then_sort_head():
    out = (
        EDGES.nodes()
        .with_columns(pr=ur.pagerank(EDGES))
        .sample(3, seed=2)
        .sort("id")
        .head(2)
        .collect()
    )
    ids = out.to_arrow().column("id").to_pylist()
    assert ids == sorted(ids) and len(ids) == 2


def test_sample_plain_node_frame_deterministic(tmp_path):
    nodes = ur.scan_nodes(_nodes_file(tmp_path), id="id")
    a = _rows(nodes.sample(2, seed=5).collect())
    b = _rows(nodes.sample(2, seed=5).collect())
    assert a == b and len(a) == 2


# --- distinct (now available on node queries) ------------------------------


def test_distinct_on_node_query_executes():
    # distinct() on a node query used to raise (it wasn't wired into the node tail);
    # the shared tail now gives it for free. A node query always carries a unique id,
    # so every (id, ...) row is already distinct — distinct is a correct no-op here,
    # and crucially no longer raises NotImplementedError.
    out = EDGES.nodes().with_columns(comp=ur.connected_components(EDGES)).distinct().collect()
    ids = out.to_arrow().column("id").to_pylist()
    assert sorted(ids) == [0, 1, 2, 3]


def test_distinct_on_plain_node_frame(tmp_path):
    # A plain (scan-backed) node frame with a genuinely duplicated row: distinct
    # collapses it. (Regression guard: distinct must not be silently dropped here.)
    path = tmp_path / "dupe_nodes.csv"
    path.write_text("id,region\n0,us\n0,us\n1,eu\n")
    out = ur.scan_nodes(str(path), id="id").distinct().collect().to_arrow()
    assert out.num_rows == 2
    assert sorted(out.column("id").to_pylist()) == [0, 1]


def test_rename_then_select_over_scan_source(tmp_path):
    # rename target must not leak into scan projection pushdown (it's a new label,
    # not a file column). Plain path.
    out = (
        ur.scan_nodes(_nodes_file(tmp_path), id="id")
        .rename({"region": "r"})
        .select("id", "r")
        .collect()
        .to_arrow()
    )
    assert out.column_names == ["id", "r"]


def test_rename_then_select_over_graph_scan_source(tmp_path):
    # Same, on the graph path with a file-backed node source: rename a computed
    # column, then select it by its new name.
    out = (
        ur.scan_nodes(_nodes_file(tmp_path), id="id")
        .with_columns(indeg=ur.degree(EDGES, direction="in"))
        .rename({"indeg": "d"})
        .select("id", "d")
        .collect()
        .to_arrow()
    )
    assert out.column_names == ["id", "d"]


def test_distinct_collapses_duplicate_rows_via_traversal():
    # A traversal output can have genuinely duplicate rows: two seeds reaching the
    # same node produce identical (src, dst) pairs after projecting to dst.
    dup = ur.hop(EDGES, 1).from_([1, 2, 3]).select("dst").collect().to_arrow()
    assert dup.column("dst").to_pylist() == [0, 0, 0]  # three identical rows
    deduped = ur.hop(EDGES, 1).from_([1, 2, 3]).distinct().select("dst").collect().to_arrow()
    # distinct on the (src, dst) pairs keeps them (distinct srcs), but the dst set is 1.
    assert set(deduped.column("dst").to_pylist()) == {0}
