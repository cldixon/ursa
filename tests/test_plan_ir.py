"""The plan IR (#190) keeps what collect() accepted and raised before it.

`collect()` serializes every step up front, so these pin the cases where that
could have changed behaviour: literals with no JSON form (typed literals of the
plan since #192), integer arguments the host's evaluator accepts but the engine
doesn't, explicit nulls, which error wins when a pipeline has two, and that a
wrapped join error keeps its cause.
"""

import datetime
import random

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

EDGES = pa.table(
    {
        "s": [0, 1, 2, 3],
        "d": [1, 2, 3, 0],
        "dur": pa.array([1, 10, 20, 3], pa.duration("s")),
        "ts": pa.array(
            [datetime.datetime(y, 1, 1) for y in (2019, 2021, 2022, 2018)], pa.timestamp("us")
        ),
        "w": [0.5, float("nan"), 2.0, 3.0],
    }
)
NODES = pa.table({"id": [0, 1, 2, 3], "v": [1, 2, 3, 4]})

# literal -> (column, rows kept: s of each edge, in order)
LITERALS = {
    "nan": (float("nan"), "w", []),
    "datetime": (datetime.datetime(2020, 1, 1), "ts", [1, 2]),
    "np_datetime64": (np.datetime64("2020-01-01", "us"), "ts", [1, 2]),
    "np_timedelta64": (np.timedelta64(5, "s"), "dur", [1, 2]),
}


def _edges():
    return ur.from_arrow(EDGES, src="s", dst="d")


@pytest.mark.parametrize("name", list(LITERALS))
def test_literals_with_no_json_form_still_filter_plain_frames(name):
    value, column, kept = LITERALS[name]
    out = _edges().filter(ur.col(column) > value).collect().to_arrow()
    assert out.column("s").to_pylist() == kept


@pytest.mark.parametrize("name", list(LITERALS))
def test_literals_with_no_json_form_still_build_subgraph_masks(name):
    value, column, kept = LITERALS[name]
    degree = ur.degree(_edges().filter(ur.col(column) > value)).collect().to_arrow()
    assert degree.column("degree").to_pylist() == [1 if s in kept else 0 for s in range(4)]


@pytest.mark.parametrize("name", list(LITERALS))
def test_literals_with_no_json_form_filter_in_the_engine_too(name):
    # #192: they are typed literals of the plan, so a filter the engine runs (here
    # after graph columns on an attribute frame) keeps the plain path's rows.
    # (`main` raised TypeError / ComputeError "invalid filter expression JSON".)
    value, column, kept = LITERALS[name]
    attrs = ur.from_arrow(EDGES, id="s").with_columns(dg=ur.degree(_edges()))
    out = attrs.filter(ur.col(column) > value).collect().to_arrow()
    assert out.column("s").to_pylist() == kept


def test_numpy_numbers_are_engine_literals():
    out = ur.degree(_edges()).filter(ur.col("degree") >= np.int64(1)).collect()
    assert out.to_arrow().num_rows == 4


@pytest.mark.parametrize("seed", [-1, "abc", 2**70])
def test_plain_sample_takes_any_seed_the_host_accepts(seed):
    out = ur.from_arrow(NODES, id="id").sample(2, seed=seed).collect().to_arrow()
    picked = sorted(random.Random(seed).sample(range(4), 2))
    assert out.column("id").to_pylist() == picked


@pytest.mark.parametrize(
    ("seed", "error", "match"),
    [
        (-1, OverflowError, "can't convert negative int to unsigned"),
        ("abc", TypeError, "'str' object cannot be interpreted as an integer"),
        (2**70, OverflowError, "int too big to convert"),
    ],
)
def test_engine_sample_seed_errors_are_unchanged(seed, error, match):
    with pytest.raises(error, match=match):
        ur.degree(_edges())._frame().sample(2, seed=seed).collect()


def test_integer_argument_errors_are_unchanged():
    e = _edges()
    with pytest.raises(OverflowError, match="out of range integral type conversion"):
        ur.hop(e, -1).from_([0]).collect()
    with pytest.raises(OverflowError, match="can't convert negative int to unsigned"):
        ur.random_walk(e, start=[0], steps=2, seed=-1).collect()
    with pytest.raises(OverflowError, match="can't convert negative int to unsigned"):
        ur.degree(e).head(-1).collect()
    assert ur.from_arrow(NODES, id="id").head(-1).collect().to_arrow().num_rows == 0


def test_graph_parameter_errors_are_engine_errors():
    e = _edges()
    with pytest.raises(ur.ComputeError, match="invalid columns spec: invalid value: integer `-1`"):
        ur.louvain(e, seed=-1).collect()
    with pytest.raises(ur.ComputeError, match='invalid columns spec: invalid type: string "x"'):
        ur.pagerank(e, damping="x").collect()  # ty: ignore[invalid-argument-type]


def test_connected_components_mode_none_is_rejected():
    with pytest.raises(NotImplementedError, match=r"connected_components\(mode=None\)"):
        ur.connected_components(_edges(), mode=None).collect()  # ty: ignore[invalid-argument-type]


def test_shape_guards_win_over_argument_errors():
    e, n = _edges(), ur.from_arrow(NODES, id="id")
    with pytest.raises(NotImplementedError, match=r"describe\(\) output does not support"):
        ur.describe(e).sort([]).collect()
    with pytest.raises(NotImplementedError, match="join how='outer' is not supported"):
        n.join(n, how="outer").collect()


@pytest.mark.parametrize("frame", ["engine", "plain"])
def test_a_graph_verb_as_a_predicate_is_not_a_boolean_predicate(frame):
    e = _edges()
    base = ur.degree(e)._frame() if frame == "engine" else e
    with pytest.raises(NotImplementedError, match=r"filter\(\) expects a boolean predicate"):
        base.filter(ur.degree(e)).collect()


@pytest.mark.parametrize("bad", ["serialize", "execute"])
def test_join_right_side_errors_keep_their_cause(bad):
    e, n = _edges(), ur.from_arrow(NODES, id="id")
    # A select of an expression fails parsing the right frame; a describe tail
    # fails its shape guard. Either way the right side's own error is the cause.
    right = n.select(ur.col("v") * 2) if bad == "serialize" else ur.describe(e).head(1)
    with pytest.raises(NotImplementedError, match="could not materialize the right-hand") as exc:
        n.join(right, on="id").collect()
    assert isinstance(exc.value.__cause__, NotImplementedError)


@pytest.mark.parametrize("source", ["memory", "scan"])
def test_graph_op_over_a_traversal_of_a_reshaped_frame_is_rejected(source, tmp_path):
    # The traversal runs over its input's index, so a reshaped input (here a
    # sample) has none to run over. A scan used to slip through and run over the
    # whole file's index.
    if source == "memory":
        e = _edges()
    else:
        path = tmp_path / "e.parquet"
        pq.write_table(EDGES.select(["s", "d"]), path)
        e = ur.scan_edges(str(path), src="s", dst="d")
    with pytest.raises(NotImplementedError, match="distinct/sample/join/group_by-derived"):
        ur.degree(ur.hop(e.sample(2), 1).from_([0])).collect()
