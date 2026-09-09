"""Layout kernels, and the multi-output machinery they are the first user of (#115).

A layout is an iterative fixpoint over the CSR — the same computational shape as
PageRank — so it is a kernel, and its positions are ordinary columns: they filter,
sort, join and `sink_parquet` like any other value.

It is also the first kernel that emits *two* columns from one invocation. The
property worth protecting there is not that `.x` and `.y` both work, but that
naming both runs the simulation **once**: the field selector is deliberately not
part of the engine's memo key.
"""

import math

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# Two triangles joined by a bridge: 0-1-2 and 3-4-5, with 2->3 between them.
BARBELL = {"s": [0, 1, 2, 2, 3, 4, 5], "d": [1, 2, 0, 3, 4, 5, 3]}


def _edges(**extra):
    return ur.from_arrow(pa.table({**BARBELL, **extra}), src="s", dst="d")


def _xy(frame):
    t = frame.collect().to_arrow()
    return t.column("x").to_pylist(), t.column("y").to_pylist()


def _laid_out(edges, **kw):
    lay = ur.layout_fr(edges, **kw)
    return edges.nodes().with_columns(x=lay.x, y=lay.y).sort("id")


def test_positions_are_two_f32_columns():
    # Two plain columns rather than a struct: struct values and unnest do not exist
    # in the dialect, and a layout is a poor reason to invent them.
    t = _laid_out(_edges(), iterations=40, seed=1).collect().to_arrow()
    assert t.schema.field("x").type == pa.float32()
    assert t.schema.field("y").type == pa.float32()
    assert t.num_rows == 6


def test_standalone_form_returns_id_x_y():
    t = ur.layout_fr(_edges(), iterations=40, seed=1).collect().to_arrow()
    assert t.schema.names == ["id", "x", "y"]


def test_x_and_y_come_from_the_same_simulation():
    """The point of #115's multi-output half.

    If `.x` and `.y` ran separate simulations they would still each be internally
    consistent, so no schema check would catch it — but the two axes would come
    from *different* runs and the drawing would be nonsense. Composing the axes
    into distances and comparing against the standalone form (which is built from
    one expression) is what actually pins them together.
    """
    edges = _edges()
    ax, ay = _xy(_laid_out(edges, iterations=60, seed=7))
    standalone = ur.layout_fr(edges, iterations=60, seed=7).collect().to_arrow()
    bx = standalone.column("x").to_pylist()
    by = standalone.column("y").to_pylist()
    assert ax == bx
    assert ay == by


def test_a_seed_reproduces_a_layout_exactly():
    edges = _edges()
    first = _xy(_laid_out(edges, iterations=50, seed=3))
    second = _xy(_laid_out(edges, iterations=50, seed=3))
    assert first == second


def test_different_seeds_differ():
    edges = _edges()
    assert _xy(_laid_out(edges, iterations=50, seed=1)) != _xy(
        _laid_out(edges, iterations=50, seed=2)
    )


def test_adjacent_nodes_end_closer_than_distant_ones():
    """The one property that makes a layout a layout rather than a scatter."""
    x, y = _xy(_laid_out(_edges(), iterations=300, seed=1))

    def d(a, b):
        return math.hypot(x[a] - x[b], y[a] - y[b])

    # 0 and 1 share an edge; 0 and 5 are in different triangles.
    assert d(0, 1) < d(0, 5)


def test_every_position_is_finite():
    # One NaN propagates through repulsion and turns the whole drawing into
    # nothing, silently.
    x, y = _xy(_laid_out(_edges(), iterations=100, seed=1))
    assert all(math.isfinite(v) for v in x)
    assert all(math.isfinite(v) for v in y)


def test_positions_compose_with_the_relational_tail():
    """Positions are columns, so the algebra applies — that is the whole claim."""
    edges = _edges()
    lay = ur.layout_fr(edges, iterations=40, seed=1)
    frame = (
        edges.nodes()
        .with_columns(x=lay.x, y=lay.y, deg=ur.degree(edges, direction="both"))
        .filter(ur.col("deg") > 2)
        .sort("id")
    )
    t = frame.collect().to_arrow()
    assert t.num_rows > 0
    assert set(t.schema.names) >= {"id", "x", "y", "deg"}


def test_layout_over_a_subgraph_view_differs():
    # A filtered edge frame is a subgraph view (#114), and laying out the subgraph
    # must draw the subgraph, not the parent.
    edges = _edges(w=[1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0])
    # Drops the bridge 2->3, leaving two disconnected triangles.
    view = edges.filter(ur.col("w") > 0.5)
    assert _xy(_laid_out(edges, iterations=60, seed=1)) != _xy(
        _laid_out(view, iterations=60, seed=1)
    )


def test_circle_places_every_node_at_one_radius():
    lay = ur.layout_circle(_edges())
    t = _edges().nodes().with_columns(x=lay.x, y=lay.y).sort("id").collect().to_arrow()
    xs, ys = t.column("x").to_pylist(), t.column("y").to_pylist()
    radii = [math.hypot(a, b) for a, b in zip(xs, ys, strict=True)]
    assert max(radii) - min(radii) < 1e-4


def test_random_is_seeded():
    edges = _edges()
    lay = ur.layout_random(edges, seed=5)
    a = edges.nodes().with_columns(x=lay.x, y=lay.y).sort("id").collect().to_arrow()
    lay2 = ur.layout_random(edges, seed=5)
    b = edges.nodes().with_columns(x=lay2.x, y=lay2.y).sort("id").collect().to_arrow()
    assert a.column("x").to_pylist() == b.column("x").to_pylist()


def _laid_out_fa2(edges, **kw):
    lay = ur.layout_fa2(edges, **kw)
    return edges.nodes().with_columns(x=lay.x, y=lay.y).sort("id")


def test_fa2_positions_are_two_f32_columns():
    t = _laid_out_fa2(_edges(), iterations=40, seed=1).collect().to_arrow()
    assert t.schema.field("x").type == pa.float32()
    assert t.schema.field("y").type == pa.float32()
    assert t.num_rows == 6


def test_fa2_x_and_y_come_from_the_same_simulation():
    """The multi-output guarantee, for the second kernel that needs it."""
    edges = _edges()
    ax, ay = _xy(_laid_out_fa2(edges, iterations=60, seed=7))
    standalone = ur.layout_fa2(edges, iterations=60, seed=7).collect().to_arrow()
    assert ax == standalone.column("x").to_pylist()
    assert ay == standalone.column("y").to_pylist()


def test_fa2_is_seeded_and_reproducible():
    edges = _edges()
    assert _xy(_laid_out_fa2(edges, iterations=50, seed=3)) == _xy(
        _laid_out_fa2(edges, iterations=50, seed=3)
    )


def test_fa2_adjacent_nodes_end_closer_than_distant_ones():
    x, y = _xy(_laid_out_fa2(_edges(), iterations=300, seed=1))

    def d(a, b):
        return math.hypot(x[a] - x[b], y[a] - y[b])

    assert d(0, 1) < d(0, 5)


def test_fa2_positions_are_finite():
    x, y = _xy(_laid_out_fa2(_edges(), iterations=100, seed=1))
    assert all(math.isfinite(v) for v in x + y)


@pytest.mark.parametrize(
    "kw",
    [
        {"strong_gravity": True},
        {"lin_log": True},
        {"theta": 0.0},
        {"theta": 1.2},
        {"jitter_tolerance": 0.1},
        {"gravity": 0.0},
    ],
)
def test_fa2_options_reach_the_kernel(kw):
    """Each flag must change the drawing.

    A parameter accepted at the Python boundary and dropped before the kernel is
    the failure this catches — the call still succeeds and the positions still look
    like positions, so nothing else would.
    """
    edges = _edges()
    base = _xy(_laid_out_fa2(edges, iterations=60, seed=1))
    tuned = _xy(_laid_out_fa2(edges, iterations=60, seed=1, **kw))
    assert base != tuned, f"{kw} did not change the layout"


def test_fa2_and_fr_are_different_kernels():
    edges = _edges()
    assert _xy(_laid_out(edges, iterations=60, seed=1)) != _xy(
        _laid_out_fa2(edges, iterations=60, seed=1)
    )


def test_fa2_over_a_subgraph_view_differs():
    edges = _edges(w=[1.0, 1.0, 1.0, 0.0, 1.0, 1.0, 1.0])
    view = edges.filter(ur.col("w") > 0.5)
    assert _xy(_laid_out_fa2(edges, iterations=60, seed=1)) != _xy(
        _laid_out_fa2(view, iterations=60, seed=1)
    )


def test_fa2_composes_with_the_relational_tail():
    edges = _edges()
    lay = ur.layout_fa2(edges, iterations=40, seed=1)
    t = (
        edges.nodes()
        .with_columns(x=lay.x, y=lay.y, deg=ur.degree(edges, direction="both"))
        .filter(ur.col("deg") > 2)
        .sort("id")
        .collect()
        .to_arrow()
    )
    assert t.num_rows > 0
    assert set(t.schema.names) >= {"id", "x", "y", "deg"}


def test_two_layouts_in_one_query_do_not_share_a_memo_entry():
    """Different kernels, and different parameters, must not collide in the memo.

    The memo keys on the algorithm and its parameters, and #115 deliberately leaves
    the field selector out of that key so x and y share a run. The risk on the other
    side is a key too *coarse*: if it omitted a parameter, two columns that differ
    only in that parameter would silently return the same simulation.
    """
    edges = _edges()
    fr = ur.layout_fr(edges, iterations=50, seed=1)
    fa2 = ur.layout_fa2(edges, iterations=50, seed=1)
    log = ur.layout_fa2(edges, iterations=50, seed=1, lin_log=True)
    t = edges.nodes().with_columns(fx=fr.x, ax=fa2.x, lx=log.x).sort("id").collect().to_arrow()
    fx, ax, lx = (t.column(c).to_pylist() for c in ("fx", "ax", "lx"))
    assert fx != ax, "layout_fr and layout_fa2 returned the same simulation"
    assert ax != lx, "lin_log was absent from the memo key"


def test_dtype_is_rejected_on_a_layout():
    # Positions are f32 at the source, so there is nothing to narrow. Accepting the
    # parameter and ignoring it would be the worse outcome.
    with pytest.raises(NotImplementedError, match="nothing to narrow"):
        edges = _edges()
        lay = ur.layout_fr(edges)
        lay.payload["dtype"] = "f32"
        edges.nodes().with_columns(x=lay.x).collect()
