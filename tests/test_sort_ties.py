"""Engine ``sort`` keeps tied rows in input order (issue #165).

A sort that runs in the native engine (after a graph algorithm or a traversal)
appends an implicit final key, the row's input position, so rows that tie on
every user key come back in the same order as the plain pyarrow path's stable
sort: node order for a graph-derived frame, the attribute table's row order for
an attribute frame, and the traversal's own output order for hop/random_walk.
The order is identical across runs and across core counts (DataFusion's
``target_partitions`` follows the CPUs the process may use).
"""

from __future__ import annotations

import os
import subprocess
import sys
import textwrap

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

N = 60_000


def _edges_table(n: int = N) -> pa.Table:
    # i -> i+1 for every i, plus i -> i+2 for even i: out-degree is 1 or 2, so half
    # the nodes tie on each value. Ids are scrambled so node (first-appearance)
    # order differs from id order.
    def ident(i: int) -> int:
        return (i * 7919) % 1_000_003

    src, dst = [], []
    for i in range(n):
        src.append(ident(i))
        dst.append(ident((i + 1) % n))
        if i % 2 == 0:
            src.append(ident(i))
            dst.append(ident((i + 2) % n))
    return pa.table({"s": src, "d": dst})


@pytest.fixture(scope="module")
def edges():
    return ur.from_arrow(_edges_table(), src="s", dst="d")


def _stable(rows: list, key) -> list:
    return sorted(rows, key=key)  # Python's sort is stable


def test_graph_derived_ties_keep_node_order(edges):
    frame = edges.nodes().with_columns(deg=ur.degree(edges, direction="out"))
    unsorted = frame.collect().to_arrow()
    node_order = unsorted.column("id").to_pylist()
    deg = dict(zip(node_order, unsorted.column("deg").to_pylist(), strict=True))
    expected = _stable(node_order, key=lambda i: -deg[i])
    # Even-position nodes (deg 2) first, then odd ones, each in node order.
    assert expected == node_order[0::2] + node_order[1::2]
    for _ in range(4):
        out = frame.sort("deg", descending=True).collect().to_arrow()
        assert out.column_names == ["id", "deg"]
        assert out.column("id").to_pylist() == expected
    # The plain (pyarrow) path over the same rows orders identically.
    plain = ur.from_arrow(unsorted, id="id").sort("deg", descending=True).collect().to_arrow()
    assert plain.column("id").to_pylist() == expected
    # With a head, the top-k keeps the same order.
    top = frame.sort("deg", descending=True).head(100).collect().to_arrow()
    assert top.column("id").to_pylist() == expected[:100]


def test_attribute_frame_ties_match_the_plain_frame(edges):
    ids = edges.nodes().with_columns(deg=ur.degree(edges)).collect().to_arrow().column("id")
    ids = ids.to_pylist()[::-1]  # attribute rows in reverse node order
    n = len(ids)
    attrs = pa.table(
        {
            "id": ids,
            "k": [i % 3 for i in range(n)],
            "c": [None if i % 7 == 0 else i % 2 for i in range(n)],
        }
    )
    attrs = pa.Table.from_batches(attrs.to_batches(max_chunksize=4096))
    frame = ur.from_arrow(attrs, id="id").with_columns(deg=ur.degree(edges, direction="out"))
    plain = ur.from_arrow(attrs, id="id")
    for by, desc in [("k", False), (["c", "k"], [True, False]), (["k", "c"], False)]:
        ref = plain.sort(by, descending=desc).collect().to_arrow().column("id").to_pylist()
        for _ in range(3):
            got = frame.sort(by, descending=desc).collect().to_arrow()
            assert got.column("id").to_pylist() == ref, (by, desc)
    # Multi-key over an attribute and the computed column, mixed directions, with
    # remaining ties on (k, deg): cross-checked against pyarrow's stable sort_by.
    deg = dict(
        zip(*frame.collect().to_arrow().select(["id", "deg"]).to_pydict().values(), strict=True)
    )
    with_deg = attrs.append_column("deg", pa.array([deg[i] for i in ids]))
    ref = with_deg.sort_by([("k", "descending"), ("deg", "ascending")]).column("id").to_pylist()
    for _ in range(3):
        got = frame.sort(["k", "deg"], descending=[True, False]).collect().to_arrow()
        assert got.column("id").to_pylist() == ref
        assert "__ursa_ord" not in got.column_names


def test_hop_ties_keep_traversal_order(edges):
    order = edges.nodes().with_columns(deg=ur.degree(edges)).collect().to_arrow()
    seeds = order.column("id").to_pylist()[:3000]
    hop = ur.hop(edges, n=2).from_(seeds)
    t = hop.collect().to_arrow()
    rows = list(zip(t.column("src").to_pylist(), t.column("dst").to_pylist(), strict=True))
    expected = _stable(rows, key=lambda r: r[0])
    for _ in range(3):
        out = hop.sort("src").collect().to_arrow()
        assert out.column_names == ["src", "dst"]
        got = list(zip(out.column("src").to_pylist(), out.column("dst").to_pylist(), strict=True))
        assert got == expected


def test_random_walk_ties_keep_walk_order(edges):
    order = edges.nodes().with_columns(deg=ur.degree(edges)).collect().to_arrow()
    walk = ur.random_walk(edges, start=order.column("id").to_pylist()[:20000], steps=4, seed=2)
    t = walk.collect().to_arrow()
    rows = list(zip(*(t.column(c).to_pylist() for c in ("walk_id", "step", "node")), strict=True))
    expected = _stable(rows, key=lambda r: -r[1])
    for _ in range(3):
        out = walk.sort("step", descending=True).collect().to_arrow()
        got = list(
            zip(*(out.column(c).to_pylist() for c in ("walk_id", "step", "node")), strict=True)
        )
        assert got == expected
    # Already in walk_id order: a stable sort by walk_id returns the rows as they are.
    for _ in range(3):
        out = walk.sort("walk_id").collect().to_arrow()
        got = list(
            zip(*(out.column(c).to_pylist() for c in ("walk_id", "step", "node")), strict=True)
        )
        assert got == rows


_CHILD = textwrap.dedent(
    """
    import hashlib, sys
    import pyarrow as pa
    import ursa as ur
    sys.path.insert(0, {tests!r})
    from test_sort_ties import _edges_table

    edges = ur.from_arrow(_edges_table(), src="s", dst="d")
    ids = edges.nodes().with_columns(deg=ur.degree(edges)).collect().to_arrow().column("id")
    attrs = pa.table({{"id": ids.to_pylist()[::-1], "k": [i % 3 for i in range(len(ids))]}})
    outs = [
        edges.nodes().with_columns(deg=ur.degree(edges)).sort("deg").collect(),
        ur.from_arrow(attrs, id="id").with_columns(deg=ur.degree(edges)).sort("k").collect(),
        ur.hop(edges, n=2).from_(ids.to_pylist()[:2000]).sort("src").collect(),
    ]
    print("|".join(hashlib.sha1(repr(o.to_arrow().to_pydict()).encode()).hexdigest() for o in outs))
    """
)


@pytest.mark.skipif(
    not hasattr(os, "sched_setaffinity") or len(os.sched_getaffinity(0)) < 2,
    reason="needs Linux CPU affinity and at least two CPUs",
)
def test_order_is_identical_across_partition_counts():
    # DataFusion sizes target_partitions from the CPUs the process may use, so
    # pinning a child process to 1 CPU vs all of them changes the partition count.
    cpus = sorted(os.sched_getaffinity(0))
    code = _CHILD.format(tests=os.path.dirname(__file__))
    digests = set()
    for subset in ({cpus[0]}, set(cpus[:2]), set(cpus)):
        out = subprocess.run(
            [sys.executable, "-c", code],
            capture_output=True,
            text=True,
            check=True,
            preexec_fn=lambda s=subset: os.sched_setaffinity(0, s),
        )
        digests.add(out.stdout.strip())
    assert len(digests) == 1
