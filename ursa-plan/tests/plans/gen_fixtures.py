"""Regenerate the plan conformance fixtures (``ursa-plan/tests/plans/*.json``).

Each case is a Python frame. ``serialize`` writes its plan, as ``collect()``
would send it to ``run_plan``, and the fixture inputs its slots bind;
``expect`` records what the frame's ``collect()`` returns (or the error it
raises). ``expect`` uses only the public API, so it can run against any build,
e.g. the release the fixtures should pin::

    python ursa-plan/tests/plans/gen_fixtures.py serialize
    python ursa-plan/tests/plans/gen_fixtures.py expect [case ...]
"""

from __future__ import annotations

import json
import sys
from collections.abc import Callable
from pathlib import Path
from typing import Any

import pyarrow as pa
import pyarrow.csv as pacsv

import ursa as ur

HERE = Path(__file__).parent
DATA = HERE.parent / "data"

# id(table) -> the fixture input that rebuilds it.
_SOURCES: dict[int, dict[str, str]] = {}


def _read(name: str) -> pa.Table:
    opts = pacsv.ConvertOptions(column_types={"weight": pa.float64()})
    return pacsv.read_csv(DATA / f"{name}.csv", convert_options=opts)


def edges(name: str) -> ur.EdgeFrame:
    table = _read(f"{name}_edges")
    frame = ur.from_arrow(table, src="src", dst="dst")
    _SOURCES[id(frame._edge_attr_table)] = {"edges": name}
    return frame


def nodes(name: str) -> ur.NodeFrame:
    frame = ur.from_arrow(_read(f"{name}_nodes"), id="id")
    _SOURCES[id(frame._attr_table)] = {"nodes": name}
    return frame


def _cases() -> dict[str, tuple[str, Callable[[], Any]]]:
    k, kn, lm, fl = edges("karate"), nodes("karate"), edges("lesmis"), edges("florentine")
    return {
        "node_query_pagerank": (
            "A standalone algorithm: ur.pagerank(edges).",
            lambda: ur.pagerank(k)._frame(),
        ),
        "node_query_composed": (
            "Two graph columns over edges.nodes(), then a filter/sort/head tail.",
            lambda: (
                k.nodes()
                .with_columns(pr=ur.pagerank(k), deg=ur.degree(k, direction="both"))
                .filter(ur.col("deg") > 5)
                .sort("pr", descending=True)
                .head(5)
            ),
        ),
        "node_query_attributes": (
            "A graph column attached to a node attribute table, filtered on an attribute.",
            lambda: (
                kn.with_columns(deg=ur.degree(k))
                .filter(ur.col("club") == "Officer")
                .sort(["deg", "id"], descending=[True, False])
                .head(4)
            ),
        ),
        "node_query_group_by": (
            "group_by().agg() over a graph column, then a sort.",
            lambda: (
                k.nodes()
                .with_columns(deg=ur.degree(k))
                .group_by("deg")
                .agg(n=ur.col("id").count())
                .sort("deg")
            ),
        ),
        "node_query_neighbors_agg": (
            "A neighbour aggregation of a string attribute.",
            lambda: kn.with_columns(
                clubs=ur.neighbors(k, direction="both").agg(ur.col("club").n_unique())
            ).head(6),
        ),
        "node_query_weighted": (
            "Weighted PageRank over Les Misérables, top five.",
            lambda: (
                ur.pagerank(lm, weight=ur.col("weight")).sort("pagerank", descending=True).head(5)
            ),
        ),
        "node_query_string_ids": (
            "Degree over the Florentine families (string ids), ties in node order.",
            lambda: ur.degree(fl, direction="both").sort("degree", descending=True).head(4),
        ),
        "join_node_query": (
            "A node query joined to the attribute table, with a tail after the join.",
            lambda: (
                k.nodes()
                .with_columns(deg=ur.degree(k))
                .join(kn, on="id")
                .filter(ur.col("deg") > 3)
                .sort("deg", descending=True)
            ),
        ),
        "hop": (
            "Two hops from node 0, filtered on the reached endpoint.",
            lambda: ur.hop(k, n=2).from_([0]).filter(ur.dst() > 30).sort(["dst", "src"]),
        ),
        "shortest_path": (
            "An unweighted shortest path, ignoring direction.",
            lambda: ur.shortest_path(k, 16, 26, direction="both"),
        ),
        "shortest_path_weighted": (
            "A weighted (Dijkstra) shortest path over string ids.",
            lambda: ur.shortest_path(lm, "Napoleon", "Javert", weight=ur.col("weight")),
        ),
        "random_walk": (
            "Seeded random walks with a tail.",
            lambda: (
                ur.random_walk(k, start=[0, 33], steps=4, walks_per_node=2, seed=7)
                .filter(ur.col("step") > 0)
                .sort(["walk_id", "step"])
            ),
        ),
        "plain_filter": (
            "A plain edge frame's filter, evaluated by the plan (#192).",
            lambda: lm.filter((ur.col("weight") >= 4) & (ur.src() != "Valjean")),
        ),
        "subgraph_degree": (
            "Degree over a subgraph view: the filter is the edge mask (#114, #192).",
            lambda: ur.degree(lm.filter(ur.col("weight") > 2), direction="both")._frame(),
        ),
        "hop_over_view": (
            "A hop over a filtered edge frame follows only the kept edges (#192).",
            lambda: ur.hop(k.filter(ur.src() > 2), n=2).from_([0, 3]),
        ),
        "degree_over_hop_of_view": (
            "A graph op over a traversal of a filtered frame: the reached region of the "
            "view, within the view (#116, #192).",
            lambda: ur.degree(ur.hop(k.filter(ur.dst() != 2), n=1).from_([0]))._frame(),
        ),
        "describe": (
            "The undirected whole-graph summary.",
            lambda: ur.describe(k, full=True, direction="both"),
        ),
        "error_describe_tail": (
            "describe() takes no tail yet.",
            lambda: ur.describe(k).head(1),
        ),
        "error_two_with_columns": (
            "One with_columns per pipeline.",
            lambda: k.nodes().with_columns(a=ur.degree(k)).with_columns(b=ur.pagerank(k)),
        ),
        "error_reshaped_edges": (
            "A graph op over a distinct edge frame.",
            lambda: ur.degree(k.distinct())._frame(),
        ),
        "error_hop_of_hop": (
            "A traversal over a traversal result.",
            lambda: ur.hop(ur.hop(k, n=1).from_([0]), n=1).from_([1]),
        ),
        "error_with_columns_after_head": (
            "A relational step before with_columns.",
            lambda: kn.head(2).with_columns(deg=ur.degree(k)),
        ),
        "error_components_mode": (
            "An unknown connected_components mode.",
            lambda: ur.connected_components(k, mode="bogus")._frame(),
        ),
        "error_join_how": (
            "An unsupported join kind.",
            lambda: kn.join(kn, on="id", how="outer"),
        ),
        "error_role_out_of_context": (
            "ur.src() in a filter over walk rows.",
            lambda: ur.random_walk(k, start=[0], steps=2, seed=1).filter(ur.src() > 0),
        ),
    }


def _input(obj: Any) -> dict[str, Any]:
    for attr in ("_edge_attr_table", "_attr_table"):
        source = _SOURCES.get(id(getattr(obj, attr, None)))
        if source is not None:
            return source
    if isinstance(obj, list):
        return {"ids": obj}
    return {"unused": True}


def _path(name: str) -> Path:
    return HERE / f"{name}.json"


def serialize() -> None:
    from ursa._execute import _Host

    for name, (description, build) in _cases().items():
        host = _Host()
        plan = host.frame(build())
        inputs = [_input(obj) for obj in host._slots]
        path = _path(name)
        expect = json.loads(path.read_text())["expect"] if path.exists() else None
        case = {"description": description, "inputs": inputs, "plan": plan, "expect": expect}
        path.write_text(json.dumps(case, indent=1) + "\n")


def expect(only: list[str] | None = None) -> None:
    errors = {
        NotImplementedError: "not_implemented",
        ur.ColumnNotFoundError: "column_not_found",
        ValueError: "value",
    }
    for name, (_description, build) in _cases().items():
        if only and name not in only:
            continue
        path = _path(name)
        case = json.loads(path.read_text())
        try:
            table = build().collect().to_arrow()
        except tuple(errors) as exc:
            kind = next(v for t, v in errors.items() if isinstance(exc, t))
            case["expect"] = {"error": kind, "message": str(exc)}
        else:
            rows = [list(row.values()) for row in table.to_pylist()]
            case["expect"] = {"columns": table.column_names, "rows": rows}
        path.write_text(json.dumps(case, indent=1) + "\n")


if __name__ == "__main__":
    if sys.argv[1] == "serialize":
        serialize()
    else:
        expect(sys.argv[2:] or None)
