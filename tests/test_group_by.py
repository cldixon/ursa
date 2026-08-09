"""group_by().agg() + aggregation expressions (issue #19).

A grouped aggregation is a new fixed point in the shared relational tail: it
runs right after any pre-group filters and *replaces* the schema with
`[keys..., outputs...]`. Two execution paths must agree on the output — the
DataFusion engine path (graph/traversal-derived frames route through
`execute_node_query`'s `group_keys`/`aggs`) and the pyarrow plain path
(`_apply_pyarrow_group_by` for source-backed frames). Every test here asserts
the grouped result directly; the cross-path test asserts both paths land on the
same numbers.

Aggregations are written `ur.col(c).<fn>()` (mean/sum/min/max/count/n_unique),
optionally `.alias(name)` or named via `.agg(name=...)`.
"""

from __future__ import annotations

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# A tiny attribute table: two regions, integer capacity + a repeated category so
# n_unique is non-trivial (us has 2 distinct tiers, eu has 1).
NODES = pa.table(
    {
        "id": [0, 1, 2, 3, 4],
        "region": ["us", "us", "us", "eu", "eu"],
        "tier": ["a", "a", "b", "c", "c"],
        "capacity": [10, 20, 30, 40, 50],
    }
)


def _nodes_file(tmp_path) -> str:
    path = tmp_path / "nodes.csv"
    path.write_text(
        "id,region,tier,capacity\n0,us,a,10\n1,us,a,20\n2,us,b,30\n3,eu,c,40\n4,eu,c,50\n"
    )
    return str(path)


def _rows_by_key(table: pa.Table, key: str) -> dict:
    """Index a small grouped result by its key column for order-independent asserts."""
    d = table.to_pydict()
    return {d[key][i]: {c: d[c][i] for c in d} for i in range(table.num_rows)}


# --- the six aggregation functions -----------------------------------------


def test_all_six_aggregations_over_a_plain_node_frame():
    out = (
        ur.from_arrow(NODES, id="id")
        .group_by("region")
        .agg(
            avg_cap=ur.col("capacity").mean(),
            total=ur.col("capacity").sum(),
            lo=ur.col("capacity").min(),
            hi=ur.col("capacity").max(),
            n=ur.col("capacity").count(),
            tiers=ur.col("tier").n_unique(),
        )
        .collect()
        .to_arrow()
    )
    assert out.num_rows == 2
    rows = _rows_by_key(out, "region")
    assert set(out.column_names) == {"region", "avg_cap", "total", "lo", "hi", "n", "tiers"}
    us = rows["us"]
    assert us["avg_cap"] == pytest.approx(20.0)  # mean(10,20,30)
    assert us["total"] == 60
    assert us["lo"] == 10
    assert us["hi"] == 30
    assert us["n"] == 3
    assert us["tiers"] == 2  # {a, b}
    eu = rows["eu"]
    assert eu["avg_cap"] == pytest.approx(45.0)  # mean(40,50)
    assert eu["total"] == 90
    assert eu["n"] == 2
    assert eu["tiers"] == 1  # {c}


# --- key forms & output-naming ---------------------------------------------


def test_group_key_accepts_col_expr_and_bare_string():
    a = ur.from_arrow(NODES, id="id").group_by("region").agg(total=ur.col("capacity").sum())
    b = ur.from_arrow(NODES, id="id").group_by(ur.col("region")).agg(total=ur.col("capacity").sum())
    assert _rows_by_key(a.collect().to_arrow(), "region") == _rows_by_key(
        b.collect().to_arrow(), "region"
    )


def test_multi_key_grouping():
    out = (
        ur.from_arrow(NODES, id="id")
        .group_by("region", "tier")
        .agg(total=ur.col("capacity").sum())
        .collect()
        .to_arrow()
    )
    # (us,a),(us,b),(eu,c) -> 3 groups
    assert out.num_rows == 3
    assert out.column_names[:2] == ["region", "tier"]
    cols = out.select(["region", "tier", "total"]).to_pydict()
    d = {(r, t): tot for r, t, tot in zip(cols["region"], cols["tier"], cols["total"], strict=True)}
    assert d[("us", "a")] == 30  # 10 + 20
    assert d[("us", "b")] == 30
    assert d[("eu", "c")] == 90  # 40 + 50


def test_output_name_precedence_keyword_alias_default():
    # keyword wins
    kw = ur.from_arrow(NODES, id="id").group_by("region").agg(mine=ur.col("capacity").sum())
    assert "mine" in kw.collect().to_arrow().column_names
    # alias when no keyword
    al = (
        ur.from_arrow(NODES, id="id")
        .group_by("region")
        .agg(ur.col("capacity").sum().alias("aliased"))
    )
    assert "aliased" in al.collect().to_arrow().column_names
    # default {column}_{fn}
    df = ur.from_arrow(NODES, id="id").group_by("region").agg(ur.col("capacity").sum())
    assert "capacity_sum" in df.collect().to_arrow().column_names


# --- composition with the rest of the tail ---------------------------------


def test_group_by_composes_with_sort_and_head():
    out = (
        ur.from_arrow(NODES, id="id")
        .group_by("region")
        .agg(total=ur.col("capacity").sum())
        .sort("total", descending=True)
        .head(1)
        .collect()
        .to_arrow()
    )
    assert out.num_rows == 1
    assert out.column("region").to_pylist() == ["eu"]  # 90 > 60


def test_group_by_composes_with_rename():
    out = (
        ur.from_arrow(NODES, id="id")
        .group_by("region")
        .agg(total=ur.col("capacity").sum())
        .rename({"total": "sum_cap"})
        .collect()
        .to_arrow()
    )
    assert "sum_cap" in out.column_names and "total" not in out.column_names


def test_pre_group_filter_is_a_where_not_having():
    # Filtering before the group is a pre-group WHERE: only capacity>15 rows count.
    out = (
        ur.from_arrow(NODES, id="id")
        .filter(ur.col("capacity") > 15)
        .group_by("region")
        .agg(total=ur.col("capacity").sum(), n=ur.col("capacity").count())
        .collect()
        .to_arrow()
    )
    rows = _rows_by_key(out, "region")
    assert rows["us"]["total"] == 50  # 20 + 30 (10 dropped)
    assert rows["us"]["n"] == 2
    assert rows["eu"]["total"] == 90


# --- plain edge frame: group by an endpoint --------------------------------


def test_group_by_on_a_plain_edge_frame():
    # out-degree via group_by(src).count over the edge table
    edges = ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2, 2, 2], "d": [1, 2, 2, 0, 1, 3]}), src="s", dst="d"
    )
    out = edges.group_by("s").agg(deg=ur.col("d").count()).collect().to_arrow()
    rows = _rows_by_key(out, "s")
    assert rows[0]["deg"] == 2
    assert rows[1]["deg"] == 1
    assert rows[2]["deg"] == 3


# --- the two execution paths must agree ------------------------------------


def test_engine_and_plain_paths_agree(tmp_path):
    # Engine path: the grouping runs over a graph-derived NodeFrame (in-degree +
    # the joined region attribute), through execute_node_query's group_keys/aggs.
    # Every node here appears in the graph, so degree is defined (never NULL) — a
    # clean cross-path comparison.
    edges = ur.from_arrow(pa.table({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}), src="s", dst="d")
    nodes = ur.from_arrow(
        pa.table({"id": [0, 1, 2, 3], "region": ["us", "us", "eu", "eu"]}), id="id"
    )
    engine = (
        nodes.with_columns(deg=ur.degree(edges, direction="in"))
        .group_by("region")
        .agg(total=ur.col("deg").sum(), n=ur.col("deg").count())
        .collect()
        .to_arrow()
    )
    # Plain path: the identical grouping straight over the attribute table, in
    # pyarrow. In-degrees by hand: node0=3, node1=1, node2=0, node3=0.
    ref = pa.table({"id": [0, 1, 2, 3], "region": ["us", "us", "eu", "eu"], "deg": [3, 1, 0, 0]})
    plain = (
        ur.from_arrow(ref, id="id")
        .group_by("region")
        .agg(total=ur.col("deg").sum(), n=ur.col("deg").count())
        .collect()
        .to_arrow()
    )
    assert _rows_by_key(engine, "region") == _rows_by_key(plain, "region")


def test_plain_matches_pyarrow_reference(tmp_path):
    # Independent oracle: our grouped sum equals a hand-rolled pyarrow group_by.
    out = (
        ur.scan_nodes(_nodes_file(tmp_path), id="id")
        .group_by("region")
        .agg(total=ur.col("capacity").sum())
        .collect()
        .to_arrow()
    )
    ref = NODES.group_by("region").aggregate([("capacity", "sum")])
    ours = _rows_by_key(out, "region")
    theirs = dict(
        zip(
            ref.column("region").to_pylist(),
            ref.column("capacity_sum").to_pylist(),
            strict=True,
        )
    )
    assert {k: v["total"] for k, v in ours.items()} == theirs


# --- error surface ---------------------------------------------------------


def test_group_by_needs_at_least_one_key():
    with pytest.raises((NotImplementedError, ValueError)):
        ur.from_arrow(NODES, id="id").group_by().agg(total=ur.col("capacity").sum()).collect()


def test_agg_needs_at_least_one_aggregation():
    with pytest.raises((NotImplementedError, ValueError)):
        ur.from_arrow(NODES, id="id").group_by("region").agg().collect()


def test_unknown_group_key_errors():
    with pytest.raises((ValueError, Exception)):
        ur.from_arrow(NODES, id="id").group_by("nope").agg(total=ur.col("capacity").sum()).collect()


def test_output_name_colliding_with_key_errors():
    with pytest.raises(ValueError):
        ur.from_arrow(NODES, id="id").group_by("region").agg(
            region=ur.col("capacity").sum()
        ).collect()


def test_duplicate_output_name_errors():
    with pytest.raises(ValueError):
        ur.from_arrow(NODES, id="id").group_by("region").agg(
            ur.col("capacity").sum().alias("x"),
            ur.col("capacity").min().alias("x"),
        ).collect()


def test_non_aggregation_expr_errors():
    with pytest.raises(NotImplementedError):
        ur.from_arrow(NODES, id="id").group_by("region").agg(ur.col("capacity")).collect()


def test_filter_after_group_by_is_rejected():
    # HAVING is not supported yet — filter after group_by must raise, not silently
    # apply as a pre-group WHERE.
    with pytest.raises(NotImplementedError):
        ur.from_arrow(NODES, id="id").group_by("region").agg(total=ur.col("capacity").sum()).filter(
            ur.col("total") > 50
        ).collect()


def test_group_by_does_not_compose_with_distinct():
    with pytest.raises(NotImplementedError):
        ur.from_arrow(NODES, id="id").group_by("region").agg(
            total=ur.col("capacity").sum()
        ).distinct().collect()


def test_group_by_on_traversal_result_is_rejected():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 3]}), src="s", dst="d")
    with pytest.raises(NotImplementedError):
        ur.hop(edges, 1).from_([0]).group_by("src").agg(n=ur.col("dst").count()).collect()


def test_unknown_agg_column_errors_on_engine_path():
    # Engine path (graph-derived frame): aggregating a column that doesn't exist
    # must raise a clear error, not an opaque DataFusion schema error — matching
    # the plain path's guard.
    edges = ur.from_arrow(pa.table({"s": [1, 2, 0], "d": [0, 0, 1]}), src="s", dst="d")
    nodes = ur.from_arrow(pa.table({"id": [0, 1, 2], "region": ["us", "us", "eu"]}), id="id")
    with pytest.raises((ValueError, Exception)) as exc:
        nodes.with_columns(deg=ur.degree(edges, direction="in")).group_by("region").agg(
            total=ur.col("missing").sum()
        ).collect()
    assert "missing" in str(exc.value)


@pytest.mark.parametrize("verb", ["sort", "head", "rename"])
def test_pre_group_sort_head_rename_is_rejected(verb):
    # A sort/head/rename written *before* the group_by would be silently reordered
    # to after it (the canonical tail order), executing a different query. Reject.
    base = ur.from_arrow(NODES, id="id")
    if verb == "sort":
        pre = base.sort("capacity")
    elif verb == "head":
        pre = base.head(2)
    else:
        pre = base.rename({"capacity": "cap"})
    with pytest.raises(NotImplementedError):
        pre.group_by("region").agg(total=ur.col("capacity").sum()).collect()


def test_duplicate_column_and_fn_distinct_output_names_agree():
    # Two aggregations sharing the same column+fn but distinct output names must
    # work (both paths emit two labels of one computation).
    out = (
        ur.from_arrow(NODES, id="id")
        .group_by("region")
        .agg(a=ur.col("capacity").sum(), b=ur.col("capacity").sum())
        .collect()
        .to_arrow()
    )
    rows = _rows_by_key(out, "region")
    assert rows["us"]["a"] == rows["us"]["b"] == 60
    assert rows["eu"]["a"] == rows["eu"]["b"] == 90
