"""The primary in-memory constructors: ``EdgeFrame(data, src=, dst=)`` and
``NodeFrame(data, id=)``.

Two layers of coverage:

* **Pure-Python** (no native extension): every supported input flavour normalizes
  to the same canonical Arrow source, and the resulting frame is byte-for-byte the
  same shape as a ``from_arrow`` frame — the property that makes "same from here
  on" true. Error cases raise clearly.
* **Native** (gated on ``ur._NATIVE_AVAILABLE``): the *five-ways-identical* test —
  the same graph built six ways (row dicts, column dict, pyarrow, polars, pandas,
  and a scanned Parquet file) yields identical ``pagerank`` results. This is the
  executable proof of the invariant, and it exercises the load-bearing plan-op
  label path end to end.
"""

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

# --- the same tiny directed graph, expressed six ways -----------------------
_SRC = [1, 2, 3, 0]
_DST = [0, 0, 0, 1]
_ROWS = [{"s": s, "d": d} for s, d in zip(_SRC, _DST, strict=True)]
_COLS = {"s": _SRC, "d": _DST}


def _pa_table():
    return pa.table(_COLS)


# ============================================================================
# Pure-Python: normalization + slot identity + errors (no native extension)
# ============================================================================


def test_constructor_accepts_row_dicts():
    edges = ur.EdgeFrame(_ROWS, src="s", dst="d")
    assert isinstance(edges, ur.EdgeFrame)
    assert edges.src_col == "s"
    assert edges.dst_col == "d"


def test_constructor_accepts_column_dict():
    edges = ur.EdgeFrame(_COLS, src="s", dst="d")
    assert isinstance(edges, ur.EdgeFrame)
    assert edges.src_col == "s"
    assert edges.dst_col == "d"


def test_constructor_accepts_pyarrow_table_and_recordbatch():
    tbl = _pa_table()
    from_tbl = ur.EdgeFrame(tbl, src="s", dst="d")
    batch = tbl.to_batches()[0]
    from_batch = ur.EdgeFrame(batch, src="s", dst="d")
    assert isinstance(from_tbl, ur.EdgeFrame)
    assert isinstance(from_batch, ur.EdgeFrame)


def test_polars_dataframe_input():
    pl = pytest.importorskip("polars")
    edges = ur.EdgeFrame(pl.DataFrame(_COLS), src="s", dst="d")
    assert isinstance(edges, ur.EdgeFrame)
    assert edges.src_col == "s"


def test_pandas_dataframe_input_drops_index():
    pd = pytest.importorskip("pandas")
    # A non-default index must NOT leak in as a stray column.
    df = pd.DataFrame(_COLS, index=[10, 11, 12, 13])
    edges = ur.EdgeFrame(df, src="s", dst="d")
    assert isinstance(edges, ur.EdgeFrame)
    assert edges._edge_table is not None
    assert edges._edge_table.schema.names == ["s", "d"]


def test_slot_identity_matches_from_arrow():
    # The whole invariant in one assertion: a constructor-built frame and a
    # from_arrow frame have the same plan and the same canonical source shape.
    a = ur.EdgeFrame(_ROWS, src="s", dst="d")
    b = ur.from_arrow(_pa_table(), src="s", dst="d")
    assert a._plan == b._plan
    assert a._plan[0].op == "from_arrow"
    assert a._edge_table is not None and b._edge_table is not None
    assert a._edge_table.schema == b._edge_table.schema
    assert a._source is not None and b._source is not None
    assert a._source[0].equals(b._source[0])
    assert a._source[1].equals(b._source[1])
    # __new__-built frames must have every slot set (fresh, independent cells).
    assert a._index_cell is not None
    assert a._index_cell is not b._index_cell


def test_all_slots_set_after_construct():
    # Guards the __new__ + __slots__ gotcha: an unset slot raises AttributeError.
    edges = ur.EdgeFrame(_ROWS, src="s", dst="d")
    for slot in (
        "_plan",
        "_has_index",
        "_src_col",
        "_dst_col",
        "_source",
        "_scan",
        "_index_cell",
        "_edge_table",
    ):
        getattr(edges, slot)  # would raise AttributeError if any were unset


def test_nodeframe_constructor_flavours():
    rows = [{"id": 0, "region": "us"}, {"id": 1, "region": "eu"}]
    cols = {"id": [0, 1], "region": ["us", "eu"]}
    for data in (rows, cols, pa.table(cols)):
        nf = ur.NodeFrame(data, id="id")
        assert isinstance(nf, ur.NodeFrame)
        assert nf.id_col == "id"


def test_bare_tuple_list_rejected():
    with pytest.raises(TypeError, match="row dicts"):
        ur.EdgeFrame([(1, 0), (2, 0)], src="s", dst="d")


def test_unsupported_type_rejected():
    with pytest.raises(TypeError, match="supported inputs"):
        ur.EdgeFrame(42, src="s", dst="d")


def test_missing_role_column_raises_at_construction():
    with pytest.raises((KeyError, ValueError, ur.ColumnNotFoundError)):
        ur.EdgeFrame(_COLS, src="nope", dst="d")


def test_from_pandas_alias():
    pd = pytest.importorskip("pandas")
    edges = ur.from_pandas(pd.DataFrame(_COLS), src="s", dst="d")
    assert isinstance(edges, ur.EdgeFrame)
    nodes = ur.from_pandas(pd.DataFrame({"id": [0, 1]}), id="id")
    assert isinstance(nodes, ur.NodeFrame)


# ============================================================================
# Native: five-ways-identical pagerank (the executable invariant proof)
# ============================================================================

_native = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _pagerank_dict(edges):
    """pagerank keyed by node id, so results are comparable across input flavours
    regardless of any internal ordering."""
    return {r["id"]: round(r["pagerank"], 12) for r in ur.pagerank(edges).collect().to_dicts()}


@_native
def test_five_ways_identical_pagerank(tmp_path):
    ways = {
        "row_dicts": ur.EdgeFrame(_ROWS, src="s", dst="d"),
        "column_dict": ur.EdgeFrame(_COLS, src="s", dst="d"),
        "pyarrow": ur.EdgeFrame(_pa_table(), src="s", dst="d"),
        "from_arrow": ur.from_arrow(_pa_table(), src="s", dst="d"),
    }

    pl = pytest.importorskip("polars")
    ways["polars"] = ur.EdgeFrame(pl.DataFrame(_COLS), src="s", dst="d")

    pd = pytest.importorskip("pandas")
    ways["pandas"] = ur.EdgeFrame(pd.DataFrame(_COLS), src="s", dst="d")

    # Sixth way: a scanned Parquet file — the scan path must agree with in-memory.
    path = tmp_path / "edges.parquet"
    pq.write_table(_pa_table(), path)
    ways["scan"] = ur.scan_edges(str(path), src="s", dst="d")

    baseline = _pagerank_dict(ways["row_dicts"])
    assert baseline  # non-empty
    for name, edges in ways.items():
        assert _pagerank_dict(edges) == baseline, f"{name} disagreed with row_dicts"
