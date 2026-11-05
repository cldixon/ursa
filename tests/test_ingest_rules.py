"""An in-memory table is read by the rules a scan uses, in Rust (#269).

``ursa._ursa.ingest_edges`` / ``ingest_nodes`` apply ``ursa_io::ids``: string
views read as strings, rows with a null endpoint dropped and counted for
``on_null="drop"``, and id columns canonicalized (integers to ``int64``, string
layouts kept, dictionaries decoded). The Python API's errors, warnings and
counts are as they were when ``python/ursa/_io.py`` applied these rules.
"""

from __future__ import annotations

import warnings

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa import _io

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _table(edges):
    table = edges._edge_table
    assert table is not None
    return table


def test_the_rules_are_not_in_python():
    for name in (
        "_without_string_views",
        "_canonical_id_array",
        "_node_attr_table",
        "_drop_null_endpoint_rows",
        "_extract_edge_arrays",
    ):
        assert not hasattr(_io, name), name


@pytest.mark.parametrize(
    ("ids", "want"),
    [
        (pa.array([0, 1, 2], pa.int8()), pa.int64()),
        (pa.array([0, 1, 2], pa.uint64()), pa.int64()),
        (pa.array(["a", "b", "c"], pa.string()), pa.string()),
        (pa.array(["a", "b", "c"], pa.large_string()), pa.large_string()),
        (pa.array(["a", "b", "c"], pa.string_view()), pa.string()),
        (pa.array(["a", "b", "a"]).dictionary_encode(), pa.string()),
        (pa.array([5, 6, 5], pa.int16()).dictionary_encode(), pa.int64()),
    ],
)
def test_endpoint_columns_are_canonical_ids(ids, want):
    rotated = pa.concat_arrays([ids[1:], ids[:1]])
    edges = ur.from_arrow(pa.table({"s": ids, "d": rotated}), src="s", dst="d")
    arrays = edges._edge_arrays
    assert arrays is not None
    src, dst = arrays
    assert src.type == dst.type == want
    nodes = ur.from_arrow(pa.table({"id": ids, "x": [1, 2, 3]}), id="id")
    assert nodes.collect().to_arrow().schema.field("id").type == want


def test_string_view_columns_read_as_strings():
    t = pa.table(
        {
            "s": pa.array(["a", "b"], pa.string_view()),
            "d": pa.array(["b", "a"], pa.string()),
            "tag": pa.array(["x", None], pa.string_view()),
        }
    )
    edges = ur.from_arrow(t, src="s", dst="d")
    assert _table(edges).schema.types == [pa.string(), pa.string(), pa.string()]
    nodes = ur.from_arrow(t.rename_columns(["id", "d", "tag"]), id="id")
    assert nodes.collect().to_arrow().schema.field("tag").type == pa.string()


def test_an_id_above_int64_raises_as_pyarrow_did():
    t = pa.table({"s": pa.array([0, 2**64 - 1], pa.uint64()), "d": pa.array([1, 0], pa.uint64())})
    with pytest.raises(pa.ArrowInvalid) as info:
        ur.from_arrow(t, src="s", dst="d")
    assert str(info.value) == (
        "Integer value 18446744073709551615 not in range: 0 to 9223372036854775807"
    )


@pytest.mark.parametrize(
    "ids",
    [
        pa.array([1.0, 2.0]),
        pa.array([{"a": 1}, {"a": 2}]),
        pa.array([1.0, 2.0]).dictionary_encode(),
        pa.array([b"a", b"b"], pa.binary_view()),
    ],
)
def test_a_column_that_cannot_hold_ids_names_its_pyarrow_type(ids):
    t = pa.table({"s": ids, "d": ids})
    with pytest.raises(TypeError) as info:
        ur.from_arrow(t, src="s", dst="d")
    assert str(info.value) == f"node ids must be an integer or string column; got {ids.type}"
    with pytest.raises(TypeError) as info:
        ur.from_arrow(pa.table({"id": ids}), id="id")
    assert str(info.value) == f"node ids must be an integer or string column; got {ids.type}"


def test_src_is_checked_before_dst():
    t = pa.table({"s": [1.5], "d": [{"a": 1}]})
    with pytest.raises(TypeError, match="got double"):
        ur.from_arrow(t, src="s", dst="d")
    with pytest.raises(KeyError, match="src column 'nope'"):
        ur.from_arrow(t, src="nope", dst="also_nope")
    # With on_null="drop" both roles are looked up before either type is read.
    with pytest.raises(KeyError, match="dst column 'nope'"):
        ur.from_arrow(t, src="s", dst="nope", on_null="drop")


def test_drop_counts_match_a_scan_of_the_same_rows(tmp_path):
    t = pa.table({"s": [0, None, 2, 3, None], "d": [1, 2, None, 0, None], "w": [1.0] * 5})
    halves = [t.slice(0, 2), t.slice(2)]
    paths = []
    for i, half in enumerate(halves):
        paths.append(str(tmp_path / f"e{i}.parquet"))
        pq.write_table(half, paths[-1])
    with warnings.catch_warnings(record=True) as caught:
        warnings.simplefilter("always")
        mem = ur.from_arrow(pa.Table.from_batches(t.to_batches()), src="s", dst="d", on_null="drop")
        scan = ur.scan_edges(paths, src="s", dst="d", on_null="drop")
        want = ur.degree(mem).collect().to_dicts()
        got = ur.degree(scan).collect().to_dicts()
    assert got == want
    messages = [str(w.message) for w in caught]
    assert messages == [
        "on_null='drop': dropped 3 edge row(s) with a null src or dst endpoint.",
        "on_null='drop': dropped 3 edge row(s) with a null src or dst endpoint.",
    ]
    assert _table(mem).num_rows == 2


def test_no_rows_dropped_keeps_the_callers_columns():
    t = pa.table({"s": [0, 1], "d": [1, 0], "w": [1.0, 2.0]})
    with warnings.catch_warnings():
        warnings.simplefilter("error")
        edges = ur.from_arrow(t, src="s", dst="d", on_null="drop")
    ours = _table(edges).column("w").chunks[0].buffers()[1]
    assert ours.address == t.column("w").chunks[0].buffers()[1].address
