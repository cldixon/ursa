"""Unsorted engine output follows input order (issue #168).

Without a ``sort``, an engine pipeline whose rows pass through a hash join or a
hash aggregate used to come back in an order that depended on DataFusion's
partition count. Those paths now number their input rows and end with an order
by that number, so unsorted output follows input order on every run and core
count, as on the plain (pyarrow) path:

- an attribute frame with graph algorithms: the attribute table's row order;
- ``join``: the left row's position, then the right row's;
- ``group_by`` / ``distinct``: the order of each group's first input row;
- ``sample``: the picked rows in input order;
- ``head(n)``: the first ``n`` rows of that order.
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
    # The tie-heavy graph of test_sort_ties.py: i -> i+1 for every i, plus i -> i+2
    # for even i, with scrambled ids.
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


@pytest.fixture(scope="module")
def attrs(edges):
    """An attribute table in a scrambled order (not node or id order), in chunks
    smaller than the engine's batch size, with a tie-heavy category ``k``."""
    ids = edges.nodes().with_columns(deg=ur.degree(edges)).collect().to_arrow().column("id")
    ids = ids.to_pylist()
    n = len(ids)
    order = sorted(range(n), key=lambda i: (i * 4241) % n)
    ids = [ids[i] for i in order]
    t = pa.table({"id": ids, "k": [(3 * i + 2) % 5 for i in range(n)], "v": list(range(n))})
    return pa.Table.from_batches(t.to_batches(max_chunksize=3000))


def _deg(edges) -> dict:
    t = edges.nodes().with_columns(deg=ur.degree(edges, direction="out")).collect().to_arrow()
    return dict(zip(t.column("id").to_pylist(), t.column("deg").to_pylist(), strict=True))


def _rows(t: pa.Table, cols: list[str]) -> list[tuple]:
    return list(zip(*(t.column(c).to_pylist() for c in cols), strict=True))


def test_attribute_frame_follows_the_table_order(edges, attrs):
    frame = ur.from_arrow(attrs, id="id").with_columns(deg=ur.degree(edges, direction="out"))
    deg = _deg(edges)
    ids = attrs.column("id").to_pylist()
    for _ in range(3):
        out = frame.collect().to_arrow()
        assert out.column_names == ["id", "k", "v", "deg"]
        assert out.column("id").to_pylist() == ids
        assert out.column("deg").to_pylist() == [deg[i] for i in ids]
    # The same rows as a plain frame come back identically.
    plain = ur.from_arrow(frame.collect().to_arrow(), id="id")
    assert plain.collect().to_arrow().equals(frame.collect().to_arrow())


def test_attribute_frame_head_filter_and_select(edges, attrs):
    frame = ur.from_arrow(attrs, id="id").with_columns(deg=ur.degree(edges, direction="out"))
    full = frame.collect().to_arrow()
    plain = ur.from_arrow(full, id="id")
    pipelines = [
        lambda f: f.head(100),
        lambda f: f.head(25_000),
        lambda f: f.filter(ur.col("k") == 2),
        lambda f: f.filter(ur.col("deg") == 2).head(500),
        lambda f: f.head(1000).filter(ur.col("k") > 2),
        lambda f: f.filter(ur.col("k") != 1).select("v", "deg").rename({"v": "w"}),
    ]
    for build in pipelines:
        want = build(plain).collect().to_arrow()
        for _ in range(2):
            assert build(frame).collect().to_arrow().equals(want)
    head = frame.head(100).collect().to_arrow()
    assert head.column("id").to_pylist() == attrs.column("id").to_pylist()[:100]


def test_group_by_and_distinct_follow_first_occurrence(edges, attrs):
    frame = ur.from_arrow(attrs, id="id").with_columns(deg=ur.degree(edges, direction="out"))
    plain = ur.from_arrow(frame.collect().to_arrow(), id="id")
    first_k = list(dict.fromkeys(attrs.column("k").to_pylist()))
    assert first_k != sorted(first_k)
    for f in (frame, plain):
        g = f.group_by("k").agg(n=ur.col("v").count(), s=ur.col("deg").sum()).collect().to_arrow()
        assert g.column("k").to_pylist() == first_k
        # After a filter, the first surviving row of each group decides.
        g = f.filter(ur.col("deg") == 1).group_by("k", "deg").agg(m=ur.col("v").min())
        g = g.collect().to_arrow()
        assert g.column("m").to_pylist() == sorted(g.column("m").to_pylist())
        # distinct: whole rows, then a projected distinct (run after the engine).
        d = f.distinct().collect().to_arrow()
        assert d.column("id").to_pylist() == attrs.column("id").to_pylist()
        d = f.select("k", "deg").distinct().collect().to_arrow()
        full = f.collect().to_arrow()
        assert _rows(d, ["k", "deg"]) == list(dict.fromkeys(_rows(full, ["k", "deg"])))


def test_sample_keeps_input_order(edges, attrs):
    frame = ur.from_arrow(attrs, id="id").with_columns(deg=ur.degree(edges, direction="out"))
    pos = {i: p for p, i in enumerate(attrs.column("id").to_pylist())}
    for f in (frame, frame.filter(ur.col("k") < 3)):
        out = f.sample(2000, seed=3).collect().to_arrow()
        positions = [pos[i] for i in out.column("id").to_pylist()]
        assert positions == sorted(positions)
        head = f.sample(2000, seed=3).head(10).collect().to_arrow()
        assert head.equals(out.slice(0, 10))


def test_graph_derived_group_by_follows_node_order(edges):
    frame = edges.nodes().with_columns(deg=ur.degree(edges, direction="out"))
    nodes = frame.collect().to_arrow()
    plain = ur.from_arrow(nodes, id="id")
    first = list(dict.fromkeys(nodes.column("deg").to_pylist()))
    for f in (frame, plain):
        g = f.group_by("deg").agg(n=ur.col("id").count()).collect().to_arrow()
        assert g.column("deg").to_pylist() == first
        d = f.distinct().collect().to_arrow()
        assert _rows(d, ["id", "deg"]) == _rows(nodes, ["id", "deg"])


def test_hop_distinct_follows_traversal_order(edges):
    order = edges.nodes().with_columns(deg=ur.degree(edges)).collect().to_arrow()
    hop = ur.hop(edges, n=2).from_(order.column("id").to_pylist()[:3000])
    rows = _rows(hop.collect().to_arrow(), ["src", "dst"])
    for _ in range(2):
        assert _rows(hop.distinct().collect().to_arrow(), ["src", "dst"]) == list(
            dict.fromkeys(rows)
        )


def _nested_loop_join(left: pa.Table, right: pa.Table, on: str, how: str) -> list[tuple]:
    """Left-major nested-loop join: left rows in order, each with its matches in
    right order (an unmatched left row once, with nulls, for how='left')."""
    rcols = [c for c in right.column_names if c != on]
    by_key: dict = {}
    for r in right.to_pylist():  # each key's rows, in right order
        by_key.setdefault(r[on], []).append(r)
    out = []
    for lrow in left.to_pylist():
        matches = by_key.get(lrow[on], [])
        if not matches and how == "left":
            out.append(tuple(lrow.values()) + (None,) * len(rcols))
        out.extend(tuple(lrow.values()) + tuple(r[c] for c in rcols) for r in matches)
    return out


@pytest.mark.parametrize("how", ["inner", "left"])
def test_join_follows_left_then_right_order(edges, attrs, how):
    # The right side has 0, 1 or 2 rows per id (ids from every third attribute row
    # are missing, ids from every fourth appear twice), in its own order.
    ids = attrs.column("id").to_pylist()
    rids = [i for p, i in enumerate(ids) if p % 3 != 0][::-1]
    rids += [i for p, i in enumerate(ids) if p % 4 == 0]
    right = pa.table({"id": rids, "w": list(range(len(rids)))})
    left = attrs.slice(0, 8000)
    want = _nested_loop_join(left, right, "id", how)
    joined = ur.from_arrow(left, id="id").join(ur.from_arrow(right, id="id"), on="id", how=how)
    for _ in range(2):
        out = joined.collect().to_arrow()
        assert out.column_names == ["id", "k", "v", "w"]
        assert _rows(out, out.column_names) == want
    assert _rows(joined.head(50).collect().to_arrow(), ["id", "k", "v", "w"]) == want[:50]
    got = joined.filter(ur.col("k") == 3).collect().to_arrow()
    assert _rows(got, ["id", "k", "v", "w"]) == [r for r in want if r[1] == 3]
    # A group_by after the join runs on the joined rows in their fixed order.
    g = joined.group_by("k").agg(n=ur.col("v").count()).collect().to_arrow()
    assert g.column("k").to_pylist() == list(dict.fromkeys(r[1] for r in want))
    # An attribute frame with an algorithm as the left side.
    algo = ur.from_arrow(left, id="id").with_columns(deg=ur.degree(edges, direction="out"))
    out = algo.join(ur.from_arrow(right, id="id"), on="id", how=how).collect().to_arrow()
    assert _rows(out, ["id", "k", "v", "w"]) == want


def test_chunked_scan_attribute_frame_with_filter(edges, attrs, tmp_path):
    import pyarrow.parquet as pq

    path = tmp_path / "nodes.parquet"
    pq.write_table(attrs, path, row_group_size=4000)
    frame = ur.scan_nodes(str(path), id="id").with_columns(deg=ur.degree(edges, direction="out"))
    want = [r["id"] for r in attrs.to_pylist() if r["k"] > 1]
    for _ in range(2):
        out = frame.filter(ur.col("k") > 1).collect().to_arrow()
        assert out.column("id").to_pylist() == want


_CHILD = textwrap.dedent(
    """
    import hashlib, sys
    import pyarrow as pa
    import ursa as ur
    sys.path.insert(0, {tests!r})
    from test_unsorted_order import _edges_table

    edges = ur.from_arrow(_edges_table(), src="s", dst="d")
    ids = edges.nodes().with_columns(deg=ur.degree(edges)).collect().to_arrow().column("id")
    ids = ids.to_pylist()[::-1]
    attrs = pa.table({{"id": ids, "k": [i % 3 for i in range(len(ids))]}})
    right = pa.table({{"id": ids[::2], "w": list(range(len(ids[::2])))}})
    frame = ur.from_arrow(attrs, id="id").with_columns(pr=ur.pagerank(edges))
    join = ur.from_arrow(attrs, id="id").join(ur.from_arrow(right, id="id"), on="id")
    outs = [
        frame.collect(),
        frame.head(100).collect(),
        frame.filter(ur.col("k") == 1).collect(),
        frame.group_by("k").agg(n=ur.col("pr").count()).collect(),
        join.collect(),
        join.head(100).collect(),
        edges.nodes().with_columns(deg=ur.degree(edges)).group_by("deg").agg(
            n=ur.col("id").count()
        ).collect(),
    ]
    print("|".join(hashlib.sha1(repr(o.to_arrow().to_pydict()).encode()).hexdigest() for o in outs))
    """
)


@pytest.mark.skipif(
    not hasattr(os, "sched_setaffinity") or len(os.sched_getaffinity(0)) < 2,
    reason="needs Linux CPU affinity and at least two CPUs",
)
def test_unsorted_order_is_identical_across_partition_counts():
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
