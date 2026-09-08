"""Kernel work shared across the columns of one query (#115).

A `with_columns(...)` naming several graph algorithms builds *one* result batch,
and columns in it can rest on the same work in two distinct ways. Both are now
shared within a query rather than recomputed per column:

  * **The same computation under two names.** Two columns naming one kernel with
    identical parameters — including the same kernel at two output dtypes, since
    `dtype=` narrows on emit and so is not part of the computation's identity.
  * **Different kernels over a shared intermediate.** `triangle_count` and
    `clustering_coefficient` both rest on one sorted-adjacency intersection pass
    over one undirected view; the clustering coefficient is a cheap derivation of
    the triangle counts.

Sharing is not directly observable from Python — the proofs that a kernel ran once
(`Arc::ptr_eq` on the emitted array, one entry in the memo, one triangle-count
allocation reused) are Rust-side unit tests in `ursa-plan/src/result.rs`. What
these tests pin is the property that matters to a user and would break loudly if
the sharing were ever wrong: a shared column must equal the value the same kernel
produces on its own. So each case computes the columns together and separately and
demands they agree exactly.
"""

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


def _graph():
    """The triangle 0-1-2, plus 3 hanging off 0 and the chord 1-3.

    Chosen so the triangle family has non-trivial values to compare: node 0 and 1
    sit in triangles, node 3 has degree 2 with one connected neighbour pair, and
    every node has a distinct clustering coefficient from at least one other.
    """
    return ur.from_arrow(pa.table({"s": [0, 1, 2, 0, 1], "d": [1, 2, 0, 3, 3]}), src="s", dst="d")


def _column(frame, name):
    return frame.collect().to_arrow().column(name).to_pylist()


def test_triangle_family_together_matches_each_alone():
    """The shared intersection pass must produce what the standalone kernels do."""
    edges = _graph()

    together = (
        edges.nodes()
        .with_columns(
            tri=ur.triangle_count(edges),
            cc=ur.clustering_coefficient(edges),
        )
        .sort("id")
    )
    tri_alone = edges.nodes().with_columns(tri=ur.triangle_count(edges)).sort("id")
    cc_alone = edges.nodes().with_columns(cc=ur.clustering_coefficient(edges)).sort("id")

    assert _column(together, "tri") == _column(tri_alone, "tri")
    # Exact equality: sharing the triangle pass must run the identical float
    # operations, not merely a close approximation of them.
    assert _column(together, "cc") == _column(cc_alone, "cc")
    # Guard against the comparison passing on a degenerate all-zero result.
    assert any(v > 0 for v in _column(together, "tri"))
    assert any(v > 0.0 for v in _column(together, "cc"))


def test_triangle_family_together_matches_each_alone_over_a_subgraph_view():
    """The same, under a mask (#114) — where the shared work is larger.

    Only the *unmasked* undirected view is cached on the topology, so before the
    sharing a masked query rebuilt that view once per triangle-family column on top
    of running the intersection pass twice.
    """
    edges = ur.from_arrow(
        pa.table(
            {
                "s": [0, 1, 2, 0, 1],
                "d": [1, 2, 0, 3, 3],
                "w": [1.0, 1.0, 1.0, 1.0, 0.0],
            }
        ),
        src="s",
        dst="d",
    )
    # Drops 1->3, which takes node 3 from two connected neighbours (cc 1.0) down to
    # one (cc 0.0) — so the mask is visible in the clustering column.
    view = edges.filter(ur.col("w") > 0.5)

    together = (
        view.nodes()
        .with_columns(tri=ur.triangle_count(view), cc=ur.clustering_coefficient(view))
        .sort("id")
    )
    tri_alone = view.nodes().with_columns(tri=ur.triangle_count(view)).sort("id")
    cc_alone = view.nodes().with_columns(cc=ur.clustering_coefficient(view)).sort("id")

    assert _column(together, "tri") == _column(tri_alone, "tri")
    assert _column(together, "cc") == _column(cc_alone, "cc")
    # The mask must actually bite, or this repeats the unmasked case.
    unmasked = edges.nodes().with_columns(cc=ur.clustering_coefficient(edges)).sort("id")
    assert _column(together, "cc") != _column(unmasked, "cc")


def test_the_same_kernel_named_twice_yields_the_same_values():
    edges = _graph()
    frame = edges.nodes().with_columns(a=ur.pagerank(edges), b=ur.pagerank(edges)).sort("id")
    assert _column(frame, "a") == _column(frame, "b")


def test_one_kernel_at_two_dtypes_agrees_within_f32_precision():
    """`dtype=` narrows on emit, so both columns come from one f64 computation."""
    import struct

    edges = _graph()
    frame = (
        edges.nodes()
        .with_columns(wide=ur.pagerank(edges), narrow=ur.pagerank(edges, dtype="f32"))
        .sort("id")
    )
    table = frame.collect().to_arrow()
    assert table.schema.field("narrow").type == pa.float32()

    def as_f32(x: float) -> float:
        return struct.unpack("f", struct.pack("f", x))[0]

    wide = table.column("wide").to_pylist()
    narrow = table.column("narrow").to_pylist()
    # Each narrow value is exactly its wide counterpart cast down — which holds only
    # if both came from the same f64 result.
    assert narrow == [as_f32(v) for v in wide]


def test_weighted_and_unweighted_forms_of_one_kernel_stay_distinct():
    """Weights are part of a computation's identity — these must not collide."""
    edges = ur.from_arrow(
        pa.table(
            {
                "s": [0, 1, 2, 0, 1],
                "d": [1, 2, 0, 3, 3],
                "w": [10.0, 1.0, 1.0, 1.0, 1.0],
            }
        ),
        src="s",
        dst="d",
    )
    frame = (
        edges.nodes()
        .with_columns(
            plain=ur.pagerank(edges),
            weighted=ur.pagerank(edges, weight=ur.col("w")),
        )
        .sort("id")
    )
    plain, weighted = _column(frame, "plain"), _column(frame, "weighted")
    assert plain != weighted, "a weighted column must not reuse the unweighted result"

    # And each matches its own standalone query.
    alone = edges.nodes().with_columns(p=ur.pagerank(edges)).sort("id")
    assert plain == _column(alone, "p")


def test_differing_parameters_do_not_share_a_result():
    edges = _graph()
    frame = (
        edges.nodes()
        .with_columns(
            d85=ur.pagerank(edges, damping=0.85),
            d50=ur.pagerank(edges, damping=0.50),
        )
        .sort("id")
    )
    assert _column(frame, "d85") != _column(frame, "d50")
