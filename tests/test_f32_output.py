"""f32 output dtype for float-valued kernel columns (#117).

A float-valued kernel (pagerank / closeness / betweenness / clustering_coefficient /
neighbours().agg()) can emit its column as 32-bit float via ``dtype="f32"``. Only the
*emitted* column narrows — the kernel still accumulates in f64 — so the value matches
the f64 result to within f32 precision. The narrowed dtype survives the whole egress
(collect / to_arrow / to_polars / sink_parquet) and the relational tail (filter/sort
with an f64 literal, the cross-type rough edge) behaves like f64 modulo precision.
"""

import math

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

native = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges():
    return ur.from_arrow(pa.table({"s": [0, 0, 1, 2, 3], "d": [1, 2, 0, 0, 0]}), src="s", dst="d")


def _col_type(batch, name):
    return batch.schema.field(name).type


# --- the emitted dtype -------------------------------------------------------
@native
@pytest.mark.parametrize(
    "expr_fn, col",
    [
        (lambda e: ur.pagerank(e, dtype="f32"), "pagerank"),
        (lambda e: ur.closeness(e, dtype="f32"), "closeness"),
        (lambda e: ur.betweenness(e, dtype="f32"), "betweenness"),
        (lambda e: ur.clustering_coefficient(e, dtype="f32"), "clustering_coefficient"),
    ],
)
def test_f32_kernel_emits_a_float32_column(expr_fn, col):
    e = _edges()
    batch = expr_fn(e).collect().to_arrow()
    assert _col_type(batch, col) == pa.float32()


@native
def test_default_dtype_is_float64():
    e = _edges()
    for expr, col in [
        (ur.pagerank(e), "pagerank"),
        (ur.closeness(e), "closeness"),
        (ur.clustering_coefficient(e), "clustering_coefficient"),
    ]:
        assert _col_type(expr.collect().to_arrow(), col) == pa.float64()


@native
def test_f32_matches_f64_within_precision():
    e = _edges()
    v32 = {r["id"]: r["pagerank"] for r in ur.pagerank(e, dtype="f32").collect().to_dicts()}
    v64 = {r["id"]: r["pagerank"] for r in ur.pagerank(e).collect().to_dicts()}
    assert v32.keys() == v64.keys()
    for k in v64:
        # f32 carries ~7 significant digits; the value is the f64 result rounded.
        assert math.isclose(v32[k], v64[k], rel_tol=1e-6, abs_tol=1e-6)


# --- the tail: filter/sort with an f64 literal (the cross-type rough edge) ----
@native
def test_filter_and_sort_over_an_f32_column_keeps_f32():
    e = _edges()
    out = (
        e.nodes()
        .with_columns(pr=ur.pagerank(e, dtype="f32"))
        .filter(ur.col("pr") > 0.1)  # 0.1 is an f64 literal vs an f32 column
        .sort("pr", descending=True)
        .collect()
        .to_arrow()
    )
    assert _col_type(out, "pr") == pa.float32()
    vals = out.column("pr").to_pylist()
    assert vals == sorted(vals, reverse=True)
    assert all(v > 0.1 for v in vals)


@native
def test_f32_matches_f64_selection_under_the_same_filter():
    # The filter picks the same nodes whether the column is f32 or f64 (no threshold
    # straddles f32's rounding here), so the tail is behaviourally identical.
    e = _edges()
    ids32 = {
        r["id"]
        for r in e.nodes()
        .with_columns(pr=ur.pagerank(e, dtype="f32"))
        .filter(ur.col("pr") > 0.15)
        .collect()
        .to_dicts()
    }
    ids64 = {
        r["id"]
        for r in e.nodes()
        .with_columns(pr=ur.pagerank(e))
        .filter(ur.col("pr") > 0.15)
        .collect()
        .to_dicts()
    }
    assert ids32 == ids64


# --- egress: to_polars / sink_parquet preserve f32 ---------------------------
@native
def test_to_polars_preserves_f32():
    import polars as pl

    e = _edges()
    df = e.nodes().with_columns(pr=ur.pagerank(e, dtype="f32")).collect().to_polars()
    assert df.schema["pr"] == pl.Float32


@native
def test_sink_parquet_preserves_f32(tmp_path):
    e = _edges()
    path = tmp_path / "pr.parquet"
    e.nodes().with_columns(pr=ur.pagerank(e, dtype="f32")).sink_parquet(str(path))
    assert pq.read_table(path).schema.field("pr").type == pa.float32()


# --- weighted + neighbour aggregation ---------------------------------------
@native
def test_weighted_pagerank_emits_f32():
    e = ur.from_arrow(
        pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 0, 0], "w": [1.0, 5.0, 1.0, 1.0]}),
        src="s",
        dst="d",
    )
    batch = ur.pagerank(e, weight=ur.col("w"), dtype="f32").collect().to_arrow()
    assert _col_type(batch, "pagerank") == pa.float32()


@native
def test_neighbor_aggregation_emits_f32():
    edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
    nodes = ur.from_arrow(pa.table({"id": [0, 1, 2], "x": [1.0, 2.0, 3.0]}), id="id")
    out = (
        nodes.with_columns(m=ur.neighbors(edges).agg(ur.col("x").mean(), dtype="f32"))
        .collect()
        .to_arrow()
    )
    assert _col_type(out, "m") == pa.float32()


# --- validation --------------------------------------------------------------
@native
def test_unknown_dtype_raises():
    e = _edges()
    with pytest.raises(NotImplementedError, match="dtype must be 'f32' or 'f64'"):
        ur.pagerank(e, dtype="f16").collect()


def test_integer_kernels_have_no_dtype_parameter():
    # degree/connected_components/... are integer-valued; they don't accept dtype at
    # all, so an f32 request is a signature error rather than a lossy silent cast.
    e = _edges()
    with pytest.raises(TypeError):
        ur.degree(e, dtype="f32")  # ty: ignore[unknown-argument]
    with pytest.raises(TypeError):
        ur.connected_components(e, dtype="f32")  # ty: ignore[unknown-argument]
