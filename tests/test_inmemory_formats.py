"""In-memory ingest formats: edge-tuple lists, networkx, numpy, scipy (issue #97).

These constructors let a user hand Ursa the data structures they already have.
Each funnels to the same ``EdgeFrame(table, src="src", dst="dst")`` path, so the
load-bearing property tested throughout is **topology equivalence**: a frame
built from any format yields the same degrees / edge set as the row-dict frame
built from the same edges. The optional deps (networkx/numpy/scipy) are import-
guarded — ``import ursa`` never pulls them in, and a missing one raises a clear
error — which we verify by simulating absence.
"""

from __future__ import annotations

import builtins

import pyarrow as pa
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

# The reference graph: 0->1, 0->2, 1->2, 2->3. Out-degrees: 0:2, 1:1, 2:1, 3:0.
EDGE_TUPLES = [(0, 1), (0, 2), (1, 2), (2, 3)]
REFERENCE = ur.from_arrow(pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 2, 3]}), src="s", dst="d")


def _out_degrees(edges) -> dict[int, int]:
    frame = edges.nodes().with_columns(deg=ur.degree(edges, direction="out"))
    d = frame.collect().to_arrow().to_pydict()
    return dict(zip(d["id"], d["deg"], strict=True))


REF_DEGREES = _out_degrees(REFERENCE)


# --- from_edgelist ---------------------------------------------------------


def test_from_edgelist_unweighted_matches_reference():
    edges = ur.from_edgelist(EDGE_TUPLES)
    assert _out_degrees(edges) == REF_DEGREES


def test_from_edgelist_weighted_carries_weight():
    edges = ur.from_edgelist([(0, 1, 2.5), (1, 2, 0.5)], weighted=True)
    tbl = edges.collect().to_arrow()
    assert "weight" in tbl.column_names
    assert tbl.column("weight").to_pylist() == [2.5, 0.5]


def test_from_edgelist_auto_detects_weight_from_arity():
    edges = ur.from_edgelist([(0, 1, 9.0), (1, 2, 8.0)])
    assert "weight" in edges.collect().to_arrow().column_names


def test_from_edgelist_string_node_ids():
    edges = ur.from_edgelist([("a", "b"), ("b", "c")])
    out = edges.collect().to_arrow()
    assert set(out.column("src").to_pylist()) == {"a", "b"}


def test_from_edgelist_weighted_in_pagerank():
    # A heavy edge shifts weighted pagerank vs the unweighted graph.
    edges = ur.from_edgelist([(0, 1, 100.0), (0, 2, 1.0), (1, 2, 1.0)], weighted=True)
    ranked = (
        edges.nodes()
        .with_columns(pr=ur.pagerank(edges, weight=ur.col("weight")))
        .collect()
        .to_arrow()
    )
    assert "pr" in ranked.column_names


def test_from_edgelist_empty_errors():
    with pytest.raises(ValueError):
        ur.from_edgelist([])


def test_from_edgelist_ragged_arity_errors():
    with pytest.raises(ValueError):
        ur.from_edgelist([(0, 1), (1, 2, 3.0)])


def test_from_edgelist_non_tuple_errors():
    with pytest.raises(TypeError):
        ur.from_edgelist([5, 6])


# --- from_networkx ---------------------------------------------------------


def test_from_networkx_matches_reference():
    nx = pytest.importorskip("networkx")
    g = nx.DiGraph(EDGE_TUPLES)
    edges = ur.from_networkx(g)
    assert _out_degrees(edges) == REF_DEGREES


def test_from_networkx_weighted():
    nx = pytest.importorskip("networkx")
    g = nx.DiGraph()
    g.add_edge(0, 1, weight=3.0)
    g.add_edge(1, 2, weight=4.0)
    tbl = ur.from_networkx(g).collect().to_arrow()
    assert "weight" in tbl.column_names
    assert sorted(tbl.column("weight").to_pylist()) == [3.0, 4.0]


def test_from_networkx_unweighted_when_partial_weights():
    nx = pytest.importorskip("networkx")
    g = nx.DiGraph()
    g.add_edge(0, 1, weight=3.0)
    g.add_edge(1, 2)  # no weight -> whole frame unweighted
    assert "weight" not in ur.from_networkx(g).collect().to_arrow().column_names


def test_nodes_from_networkx_attributes():
    nx = pytest.importorskip("networkx")
    g = nx.Graph()
    g.add_node(0, club="Mr Hi")
    g.add_node(1, club="Officer")
    nodes = ur.nodes_from_networkx(g).collect().to_arrow()
    assert set(nodes.column_names) == {"id", "club"}
    d = dict(zip(nodes.column("id").to_pylist(), nodes.column("club").to_pylist(), strict=True))
    assert d == {0: "Mr Hi", 1: "Officer"}


def test_nodes_from_networkx_id_collision_errors():
    nx = pytest.importorskip("networkx")
    g = nx.Graph()
    g.add_node(0, id="oops")
    with pytest.raises(ValueError):
        ur.nodes_from_networkx(g)


# --- from_numpy ------------------------------------------------------------


def test_from_numpy_adjacency_matches_reference():
    np = pytest.importorskip("numpy")
    adj = np.zeros((4, 4))
    for s, d in EDGE_TUPLES:
        adj[s, d] = 1
    edges = ur.from_numpy(adj, kind="adjacency")
    assert _out_degrees(edges) == REF_DEGREES


def test_from_numpy_adjacency_weighted():
    np = pytest.importorskip("numpy")
    adj = np.array([[0, 2.0], [0, 0]])
    tbl = ur.from_numpy(adj, kind="adjacency", weighted=True).collect().to_arrow()
    assert tbl.column("weight").to_pylist() == [2.0]


def test_from_numpy_edge_array():
    np = pytest.importorskip("numpy")
    arr = np.array(EDGE_TUPLES)
    edges = ur.from_numpy(arr, kind="edges")
    assert _out_degrees(edges) == REF_DEGREES


def test_from_numpy_edge_array_with_weight_column():
    np = pytest.importorskip("numpy")
    arr = np.array([[0, 1, 5.0], [1, 2, 6.0]])
    tbl = ur.from_numpy(arr, kind="edges").collect().to_arrow()
    assert tbl.column("weight").to_pylist() == [5.0, 6.0]


def test_from_numpy_auto_picks_adjacency_for_square():
    np = pytest.importorskip("numpy")
    adj = np.zeros((4, 4))
    adj[0, 1] = 1
    edges = ur.from_numpy(adj)  # auto
    assert edges.collect().to_arrow().num_rows == 1


def test_from_numpy_auto_picks_edges_for_nx2():
    np = pytest.importorskip("numpy")
    edges = ur.from_numpy(np.array([[0, 1], [1, 2], [2, 3]]))  # (3,2) -> edges
    assert edges.collect().to_arrow().num_rows == 3


def test_from_numpy_bad_shape_errors():
    np = pytest.importorskip("numpy")
    with pytest.raises(ValueError):
        ur.from_numpy(np.zeros((3, 5)))  # neither square nor 2/3-col


def test_from_numpy_non_2d_errors():
    np = pytest.importorskip("numpy")
    with pytest.raises(ValueError):
        ur.from_numpy(np.array([1, 2, 3]))


# --- from_scipy_sparse -----------------------------------------------------


def test_from_scipy_sparse_matches_reference():
    sp = pytest.importorskip("scipy.sparse")
    np = pytest.importorskip("numpy")
    rows = np.array([s for s, _ in EDGE_TUPLES])
    cols = np.array([d for _, d in EDGE_TUPLES])
    mat = sp.coo_matrix((np.ones(len(rows)), (rows, cols)), shape=(4, 4))
    edges = ur.from_scipy_sparse(mat)
    assert _out_degrees(edges) == REF_DEGREES


def test_from_scipy_sparse_csr_and_weighted():
    sp = pytest.importorskip("scipy.sparse")
    np = pytest.importorskip("numpy")
    mat = sp.csr_matrix(np.array([[0, 2.0], [3.0, 0]]))
    tbl = ur.from_scipy_sparse(mat, weighted=True).collect().to_arrow()
    assert sorted(tbl.column("weight").to_pylist()) == [2.0, 3.0]


def test_from_scipy_sparse_wrong_type_errors():
    with pytest.raises(TypeError):
        ur.from_scipy_sparse([[0, 1], [1, 0]])  # a plain list, not sparse


def test_from_scipy_sparse_wrong_type_errors_even_without_scipy(monkeypatch):
    # The non-sparse TypeError must fire before any scipy import — a wrong-typed
    # arg never needs scipy, so it should not demand it be installed.
    real_import = builtins.__import__

    def fake_import(name, *args, **kwargs):
        if name == "scipy" or name.startswith("scipy."):
            raise ImportError("No module named 'scipy'")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", fake_import)
    with pytest.raises(TypeError):
        ur.from_scipy_sparse([[0, 1], [1, 0]])


# --- empty-graph and argument-consistency edge cases -----------------------


def test_from_networkx_empty_graph_yields_empty_frame():
    nx = pytest.importorskip("networkx")
    edges = ur.from_networkx(nx.DiGraph())
    assert edges.collect().to_arrow().num_rows == 0


def test_nodes_from_networkx_empty_graph_yields_empty_frame():
    nx = pytest.importorskip("networkx")
    nodes = ur.nodes_from_networkx(nx.Graph())
    assert nodes.collect().to_arrow().num_rows == 0


def test_from_numpy_edges_weighted_true_two_columns_errors():
    np = pytest.importorskip("numpy")
    with pytest.raises(ValueError, match="3-column"):
        ur.from_numpy(np.array([[0, 1], [1, 2]]), kind="edges", weighted=True)


# --- optional-dependency isolation -----------------------------------------


def test_missing_optional_dep_raises_clear_error(monkeypatch):
    # Simulate networkx not being installed: the guarded import must raise a clear,
    # actionable ImportError naming the package — not an opaque failure.
    real_import = builtins.__import__

    def fake_import(name, *args, **kwargs):
        if name == "networkx" or name.startswith("networkx."):
            raise ImportError("No module named 'networkx'")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", fake_import)
    with pytest.raises(ImportError, match="networkx"):
        ur.from_networkx(object())


def test_import_ursa_does_not_import_optional_deps():
    # `import ursa` must not drag in the heavy scientific stack. We can't unimport
    # them mid-session, so assert the source discipline: no top-level import of the
    # optional libs in the interop module (they're imported inside functions).
    import inspect

    from ursa import _interop

    src = inspect.getsource(_interop)
    # The only mentions of these libs should be inside function bodies / strings,
    # never a module-level `import numpy` / `import networkx` / `import scipy`.
    for line in src.splitlines():
        stripped = line.strip()
        if stripped.startswith(("import ", "from ")):
            assert "numpy" not in stripped
            assert "networkx" not in stripped
            assert "scipy" not in stripped
