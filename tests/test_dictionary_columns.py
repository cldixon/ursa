"""Dictionary-encoded (categorical) columns through the plain-frame relational
tail (issue #159).

pyarrow has no sort kernel for dictionary arrays and no hash sum/mean/min/max
kernels for them, so the pyarrow tail (`_apply_pyarrow_tail`) decodes the sort
key / aggregate operand while leaving the user's column type intact in the
output. Sorting orders by the *values* (lexicographic for strings), never by the
dictionary indices: the fixtures below put "y" before "x" in the dictionary so an
index-order sort would come out wrong.
"""

from __future__ import annotations

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _dict(values: list) -> pa.Array:
    return pa.array(values).dictionary_encode()


# Dictionary order is ["y", "x"] / [20, 10]: the opposite of value order.
NODES = pa.table(
    {
        "id": [1, 2, 3, 4],
        "club": _dict(["y", "x", None, "x"]),
        "k": _dict([20, 10, None, 10]),
        "w": [1.0, 2.0, 3.0, 4.0],
    }
)
EDGES = pa.table(
    {
        "src": [1, 2, 3, 4],
        "dst": [2, 3, 4, 1],
        "club": _dict(["y", "x", None, "x"]),
        "k": _dict([20, 10, None, 10]),
        "w": [1.0, 2.0, 3.0, 4.0],
    }
)


def _node_frames(tmp_path) -> list[ur.NodeFrame]:
    path = tmp_path / "nodes.parquet"
    pq.write_table(NODES, path)
    # Parquet keeps the string dictionary type in its stored Arrow schema.
    assert pa.types.is_dictionary(pq.read_schema(path).field("club").type)
    return [ur.NodeFrame(NODES, id="id"), ur.scan_nodes(str(path), id="id")]


@pytest.mark.parametrize("col", ["club", "k"])
def test_sort_by_dictionary_column_orders_by_value(tmp_path, col):
    for nf in _node_frames(tmp_path):
        asc = nf.sort(col).collect().to_arrow()
        assert asc.column("id").to_pylist() == [2, 4, 1, 3]  # nulls last, stable
        # The output keeps the user's dictionary type.
        assert pa.types.is_dictionary(asc.schema.field(col).type)
        desc = nf.sort(col, descending=True).collect().to_arrow()
        assert desc.column("id").to_pylist() == [1, 2, 4, 3]
        top = nf.sort(col).head(2).collect().to_arrow()
        assert top.column("club").to_pylist() == ["x", "x"]


def test_issue_159_repro():
    n = pa.table({"id": [1, 2, 3], "club": _dict(["x", "y", "x"])})
    nf = ur.NodeFrame(n, id="id")
    assert nf.filter(ur.col("club") == "x").collect().to_arrow().num_rows == 2
    out = nf.sort("club", descending=True).collect().to_arrow()
    assert out.column("club").to_pylist() == ["y", "x", "x"]


def test_sort_dictionary_after_sample_distinct_rename(tmp_path):
    for nf in _node_frames(tmp_path):
        sampled = nf.sample(3, seed=1).sort("club").collect().to_arrow()
        assert sampled.column("club").to_pylist() == sorted(
            sampled.column("club").to_pylist(), key=lambda v: (v is None, v)
        )
        distinct = nf.distinct().sort("club").collect().to_arrow()
        assert distinct.column("club").to_pylist() == ["x", "x", "y", None]
        renamed = nf.sort("club").rename({"club": "c"}).collect().to_arrow()
        assert renamed.column("c").to_pylist() == ["x", "x", "y", None]


def test_sort_dictionary_multi_chunk_differing_dictionaries():
    table = pa.concat_tables(
        [
            pa.table({"id": [1, 2], "club": _dict(["b", "a"])}),
            pa.table({"id": [3, 4], "club": _dict(["c", "a"])}),
        ]
    )
    out = ur.NodeFrame(table, id="id").sort("club").collect().to_arrow()
    assert out.column("id").to_pylist() == [2, 4, 1, 3]
    assert out.column("club").to_pylist() == ["a", "a", "b", "c"]


@pytest.mark.parametrize("col", ["club", "k"])
def test_group_by_dictionary_key_then_sort(tmp_path, col):
    for nf in _node_frames(tmp_path):
        out = (
            nf.group_by(col)
            .agg(n=ur.col("w").count(), s=ur.col("w").sum())
            .sort(col)
            .collect()
            .to_arrow()
        )
        assert out.column("n").to_pylist() == [2, 1, 1]
        assert out.column("s").to_pylist() == [6.0, 1.0, 3.0]
        assert pa.types.is_dictionary(out.schema.field(col).type)


def test_aggregate_over_dictionary_column(tmp_path):
    for nf in _node_frames(tmp_path):
        out = (
            nf.group_by("id")
            .agg(
                mx=ur.col("club").max(),
                cnt=ur.col("club").count(),
                ksum=ur.col("k").sum(),
                kmean=ur.col("k").mean(),
            )
            .sort("id")
            .collect()
            .to_arrow()
        )
        assert out.column("mx").to_pylist() == ["y", "x", None, "x"]
        assert out.column("cnt").to_pylist() == [1, 1, 0, 1]
        assert out.column("ksum").to_pylist() == [20, 10, None, 10]
        assert out.column("kmean").to_pylist() == [20.0, 10.0, None, 10.0]


def test_aggregate_over_dictionary_group_key_itself():
    nf = ur.NodeFrame(NODES, id="id")
    out = (
        nf.group_by("club")
        .agg(mn=ur.col("club").min(), nu=ur.col("club").n_unique(), kmin=ur.col("k").min())
        .sort("club", descending=True)
        .collect()
        .to_arrow()
    )
    assert out.column("club").to_pylist() == ["y", "x", None]
    assert out.column("mn").to_pylist() == ["y", "x", None]
    assert out.column("nu").to_pylist() == [1, 1, 0]
    assert out.column("kmin").to_pylist() == [20, 10, None]
    assert pa.types.is_dictionary(out.schema.field("club").type)


def test_edge_frame_dictionary_sort_and_group():
    ef = ur.EdgeFrame(EDGES, src="src", dst="dst")
    out = ef.sort("club").collect().to_arrow()
    assert out.column("src").to_pylist() == [2, 4, 1, 3]
    out = ef.filter(ur.src() > 1).sort("k", descending=True).collect().to_arrow()
    assert out.column("src").to_pylist() == [2, 4, 3]
    grouped = (
        ef.group_by("club")
        .agg(n=ur.col("w").count(), kmax=ur.col("k").max())
        .sort("club")
        .collect()
        .to_arrow()
    )
    assert grouped.column("club").to_pylist() == ["x", "y", None]
    assert grouped.column("n").to_pylist() == [2, 1, 1]
    assert grouped.column("kmax").to_pylist() == [10, 20, None]


@pytest.mark.parametrize("right_club", [pa.array(["x", "y"]), _dict(["x", "y"])])
def test_join_on_dictionary_key_then_group_and_sort(right_club):
    left = ur.NodeFrame(NODES, id="id")
    right = ur.NodeFrame(
        pa.table({"rid": [1, 2], "club": right_club, "label": ["X", "Y"]}), id="rid"
    ).select("club", "label")
    joined = left.join(right, on="club").sort("club", descending=True).collect().to_arrow()
    assert joined.column("id").to_pylist() == [1, 2, 4]
    assert joined.column("label").to_pylist() == ["Y", "X", "X"]
    grouped = (
        left.join(right, on="club")
        .group_by("club")
        .agg(n=ur.col("w").count())
        .sort("club")
        .collect()
        .to_arrow()
    )
    assert grouped.column("club").to_pylist() == ["x", "y"]
    assert grouped.column("n").to_pylist() == [2, 1]
