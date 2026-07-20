"""End-to-end tests for the ``random_walk`` verb (Python → PyO3 → ursa-core).

``ur.random_walk(edges, start, steps, walks_per_node, seed)`` returns a lazy
NodeFrame of ``(walk_id, step, node)``. Skipped if the native extension has not
been built.
"""

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _edges(src, dst):
    return ur.from_arrow(pa.table({"src": src, "dst": dst}), src="src", dst="dst")


def _line():
    # 0 -> 1 -> 2 -> 3: one out-edge at each step, so a walk is forced.
    return _edges([0, 1, 2], [1, 2, 3])


def test_forced_walk_follows_the_only_path():
    rows = ur.random_walk(_line(), start=[0], steps=3, seed=42).collect().to_dicts()
    assert rows == [
        {"walk_id": 0, "step": 0, "node": 0},
        {"walk_id": 0, "step": 1, "node": 1},
        {"walk_id": 0, "step": 2, "node": 2},
        {"walk_id": 0, "step": 3, "node": 3},
    ]


def test_walk_stops_at_a_dead_end():
    # Node 3 has no out-edge; a walk from 2 emits only steps 0 and 1.
    rows = ur.random_walk(_line(), start=[2], steps=5, seed=1).collect().to_dicts()
    assert [r["node"] for r in rows] == [2, 3]


def test_multiple_walks_per_node_get_distinct_ids():
    walk = ur.random_walk(_line(), start=[0], steps=2, walks_per_node=3, seed=7)
    rows = walk.collect().to_dicts()
    assert sorted({r["walk_id"] for r in rows}) == [0, 1, 2]


def test_walk_is_deterministic_given_a_seed():
    # A branching graph so the RNG actually chooses at each step.
    edges = _edges([0, 0, 0, 1, 2, 3], [1, 2, 3, 0, 0, 0])
    a = ur.random_walk(edges, start=[0], steps=6, walks_per_node=4, seed=99).collect().to_dicts()
    b = ur.random_walk(edges, start=[0], steps=6, walks_per_node=4, seed=99).collect().to_dicts()
    assert a == b


def test_walk_composes_with_a_tail():
    # The relational tail runs on the walk rows (here: keep step 0 only).
    df = ur.random_walk(_line(), start=[0], steps=3, seed=3).filter(ur.col("step") == 0).collect()
    rows = df.to_dicts()
    assert len(rows) == 1
    assert rows[0] == {"walk_id": 0, "step": 0, "node": 0}


def test_walk_starts_can_be_a_nodeframe():
    # start= accepts a NodeFrame's ids, like ur.hop(...).from_(...).
    line = _line()
    seeds = ur.from_arrow(pa.table({"id": [0]}), id="id")
    rows = ur.random_walk(line, start=seeds, steps=1, seed=5).collect().to_dicts()
    assert rows[0]["node"] == 0
    assert {r["node"] for r in rows} <= {0, 1}
