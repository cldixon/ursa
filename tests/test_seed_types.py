"""Which values a traversal takes as node ids (#271).

A seed or endpoint (``hop(...).from_()``, ``random_walk(start=)``,
``shortest_path(source=, target=)``) is an integer or a string, as in SQL.
Whether it fits the graph's id type is decided when the plan runs: on a graph of
integer ids a string that spells an integer exactly (``'7'``) is that integer,
and on a graph of string ids an integer is its string. A float, a bool or any
other value is refused where the traversal is called, whatever the graph's id
type; it used to be converted with ``int()`` (``1.5`` ran as ``1``, ``True`` as
``1``).
"""

from __future__ import annotations

import datetime
import decimal

import numpy as np
import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _ints():
    return ur.from_arrow(pa.table({"s": [0, 1, 2, 2, 3], "d": [1, 2, 0, 3, 4]}), src="s", dst="d")


def _strings():
    return ur.from_arrow(
        pa.table({"s": ["0", "1", "2", "2", "3"], "d": ["1", "2", "0", "3", "4"]}),
        src="s",
        dst="d",
    )


def _rows(frame):
    return frame.collect().to_dicts()


@pytest.mark.parametrize(
    "seeds",
    [
        ["1", "2"],
        ["1", 2],
        (1, "2"),
        np.array([1, 2]),
        pa.array([1, 2]),
        pa.array(["1", "2"]),
        [np.int32(1), np.uint8(2)],
        (s for s in [1, 2]),
    ],
)
def test_integer_graph_seeds(seeds):
    want = _rows(ur.hop(_ints(), 1).from_([1, 2]))
    assert want
    assert _rows(ur.hop(_ints(), 1).from_(seeds)) == want


@pytest.mark.parametrize("seeds", [[1, 2], ["1", 2], np.array([1, 2]), pa.array([1, 2])])
def test_string_graph_takes_integers_as_their_strings(seeds):
    want = _rows(ur.hop(_strings(), 1).from_(["1", "2"]))
    assert want
    assert _rows(ur.hop(_strings(), 1).from_(seeds)) == want


def test_a_node_frame_of_integers_seeds_a_string_graph():
    nodes = ur.from_arrow(pa.table({"id": [1, 2]}), id="id")
    assert _rows(ur.hop(_strings(), 1).from_(nodes)) == _rows(
        ur.hop(_strings(), 1).from_(["1", "2"])
    )


def test_shortest_path_and_random_walk_fit_ids_too():
    assert _rows(ur.shortest_path(_ints(), "0", "3")) == _rows(ur.shortest_path(_ints(), 0, 3))
    assert _rows(ur.shortest_path(_strings(), 0, 3)) == _rows(
        ur.shortest_path(_strings(), "0", "3")
    )
    walk = _rows(ur.random_walk(_ints(), ["1"], 3, seed=7))
    assert walk == _rows(ur.random_walk(_ints(), [1], 3, seed=7))


@pytest.mark.parametrize(
    "bad",
    [
        1.5,
        2.0,
        np.float64(1.0),
        True,
        np.True_,
        datetime.date(2026, 1, 1),
        decimal.Decimal(1),
        None,
    ],
)
@pytest.mark.parametrize("graph", [_ints, _strings])
def test_values_that_are_not_ids_are_refused(bad, graph):
    with pytest.raises(TypeError, match=r"hop\(\.\.\.\)\.from_\(\) must be node ids"):
        ur.hop(graph(), 1).from_([bad])
    with pytest.raises(TypeError, match=r"random_walk\(start=\) must be node ids"):
        ur.random_walk(graph(), [bad], 2)
    with pytest.raises(TypeError, match=r"shortest_path\(target=\) must be a node id"):
        ur.shortest_path(graph(), 0, bad)


def test_a_string_that_is_not_an_integer_is_refused_on_an_integer_graph():
    for seeds in (["x"], [1, "2.0"], [" 1"]):
        with pytest.raises(ur.ComputeError, match="is not an integer, but the graph's node ids"):
            ur.hop(_ints(), 1).from_(seeds).collect()
