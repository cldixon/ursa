"""The Python signatures agree with the graph-function catalog in ``ursa-core``.

The catalog is the single source of truth for every graph function's parameters
and defaults, shared with the SQL engine. Python keeps its own signatures (for
``help()`` and type checkers), so this pins them to the catalog: a parameter
added, renamed, reordered or given another default on one side fails here.
"""

from __future__ import annotations

import inspect

import pyarrow as pa

import ursa as ur
from ursa import _graph
from ursa._execute import _native


def _python_params(fn) -> list[tuple[str, object]]:
    return [
        (p.name, p.default)
        for p in inspect.signature(fn).parameters.values()
        # The graph is the first argument; `lazy` picks the stats' return type.
        if p.name not in ("edges", "lazy")
    ]


def test_every_python_signature_matches_the_catalog() -> None:
    catalog = dict(_native()._graph_functions())
    for name, params in catalog.items():
        if name == "neighbors_agg":
            continue
        # The layouts are not exported yet; they live in `ursa._graph`.
        got = _python_params(getattr(ur, name, None) or getattr(_graph, name))
        assert got == params, name


def test_neighbour_aggregation_matches_the_catalog() -> None:
    # Spelled ur.neighbors(edges, direction, from_).agg(expr, dtype): the
    # attribute and the aggregation come from the expression.
    params = dict(dict(_native()._graph_functions())["neighbors_agg"])
    assert inspect.signature(ur.neighbors).parameters["direction"].default == params["direction"]
    agg = inspect.signature(
        ur.neighbors(ur.from_arrow(pa.table({"s": [0], "d": [1]}), src="s", dst="d")).agg
    )
    assert agg.parameters["dtype"].default == params["dtype"]


def test_the_catalog_covers_every_graph_function() -> None:
    names = {name for name, _ in _native()._graph_functions()}
    assert names == {
        "degree",
        "pagerank",
        "connected_components",
        "triangle_count",
        "clustering_coefficient",
        "betweenness",
        "closeness",
        "label_propagation",
        "louvain",
        "layout_fr",
        "layout_random",
        "layout_circle",
        "neighbors_agg",
        "density",
        "avg_path_length",
        "diameter",
        "describe",
    }
