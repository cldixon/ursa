"""Side-by-side parity harness for #213 / #210: the surfaces the change touches,
run on two builds and compared.

    python parity_213.py run OUT.jsonl          # with each build's interpreter
    python parity_213.py compare MAIN.jsonl BRANCH.jsonl

The cases are the eager stats (`ur.density`, `ur.avg_path_length`,
`ur.diameter`, with their arguments, right and wrong) and `ur.describe` over
every edge frame of `parity_191_reshaped` (each reshape, from memory and from a
scan), over traversal results and over frames a stat does not take; a bare
`edges.nodes()` and the steps after it; and standalone graph verbs with each
frame method. Each record is the value (a float by its exact bits), the
collected table (as `parity_191` encodes it) or the error (class and message).

`compare` sorts every difference between the builds into a known category
(anything uncategorized is a bug), and checks the branch's masked stats against
an independent reference: over a subgraph view an exact path stat equals the
same stat over a re-ingest of the view's edges, and `density` equals
`describe().density`.
"""

from __future__ import annotations

import collections
import json
import struct
import sys
import warnings
from pathlib import Path

import pyarrow as pa
import pyarrow.compute as pc

sys.path.insert(0, str(Path(__file__).resolve().parent))
from parity_191 import encode, load
from parity_191_reshaped import EDGES, RESHAPES, sources

import ursa as ur

warnings.simplefilter("ignore")
col = ur.col


def _value(v) -> dict:
    if isinstance(v, float):
        return {"float": struct.pack("<d", v).hex()}
    if isinstance(v, (bool, int)):
        return {type(v).__name__: v}
    return {"table": encode(v.collect().to_arrow())}


def _record(name: str, build) -> dict:
    try:
        out = _value(build())
    except Exception as exc:
        out = {"error": type(exc).__name__, "message": str(exc)}
    return {"case": name, **out}


STATS: dict = {
    "density": lambda e: ur.density(e),
    "density_in": lambda e: ur.density(e, "in"),
    "density_both": lambda e: ur.density(e, direction="both"),
    "apl": lambda e: ur.avg_path_length(e),
    "apl_in": lambda e: ur.avg_path_length(e, direction="in"),
    "apl_both": lambda e: ur.avg_path_length(e, direction="both"),
    "apl_sample": lambda e: ur.avg_path_length(e, sample=0.3),
    "apl_sample_both": lambda e: ur.avg_path_length(e, 0.5, "both"),
    "diam": lambda e: ur.diameter(e),
    "diam_exact": lambda e: ur.diameter(e, approximate=False),
    "diam_exact_in": lambda e: ur.diameter(e, False, "in"),
    "diam_exact_both": lambda e: ur.diameter(e, approximate=False, direction="both"),
    "describe": lambda e: ur.describe(e),
    "describe_full_both": lambda e: ur.describe(e, full=True, direction="both"),
}

# Arguments a stat does not take (over a plain frame).
BAD_ARGS: dict = {
    "density_dir": lambda e: ur.density(e, "sideways"),
    "density_dir_int": lambda e: ur.density(e, 1),
    "apl_sample_str": lambda e: ur.avg_path_length(e, sample="x"),
    "apl_sample_big": lambda e: ur.avg_path_length(e, sample=2.0),
    "apl_sample_neg": lambda e: ur.avg_path_length(e, sample=-1.0),
    "apl_sample_zero": lambda e: ur.avg_path_length(e, sample=0.0),
    "apl_sample_nan": lambda e: ur.avg_path_length(e, sample=float("nan")),
    "apl_sample_int": lambda e: ur.avg_path_length(e, sample=1),
    "diam_approx_int": lambda e: ur.diameter(e, approximate=1),
    "diam_approx_none": lambda e: ur.diameter(e, approximate=None),
    "diam_dir": lambda e: ur.diameter(e, direction="up"),
    "describe_dir": lambda e: ur.describe(e, direction="x"),
}

# Frames a stat reads that are not plain edge frames.
OTHER_FRAMES: dict = {
    "hop": lambda e: ur.hop(e, 2).from_([0, 3, 21]),
    "hop_both": lambda e: ur.hop(e, 1, "both").from_([5, 6]),
    "hop_of_filter": lambda e: ur.hop(e.filter(col("w") > 1.0), 2).from_([0, 3]),
    "hop_tail": lambda e: ur.hop(e, 2).from_([0, 3]).head(3),
    "shortest_path": lambda e: ur.shortest_path(e, 3, 26),
    "walk": lambda e: ur.random_walk(e, start=[0, 1], steps=3, seed=1),
    "nodes": lambda e: e.nodes(),
    "node_attrs": lambda e: ur.from_arrow(pa.table({"id": [1, 2]}), id="id"),
    "describe_frame": lambda e: ur.describe(e),
    "verb": lambda e: ur.degree(e),
    "edge_graph_column": lambda e: e.with_columns(x=ur.degree(e)),
    "drop_dst": lambda e: e.select("s", "w"),
    "list": lambda e: [1, 2],
}

# Frame methods on a standalone verb, and the steps after a bare nodes().
FRAME_STEPS: dict = {
    "collect": lambda f: f,
    "filter": lambda f: f.filter(col("id") > 5),
    "sort": lambda f: f.sort("id", descending=True),
    "head": lambda f: f.head(4),
    "limit": lambda f: f.limit(4),
    "select": lambda f: f.select("id"),
    "with_columns": lambda f: f.with_columns(z=col("id") + 1),
    "distinct": lambda f: f.distinct(),
    "rename": lambda f: f.rename({"id": "node"}),
    "sample": lambda f: f.sample(5, seed=2),
    "group_by": lambda f: f.group_by("id").agg(n=col("id").count()),
    "join": lambda f: f.join(ur.from_arrow(pa.table({"id": [3, 4, 5]}), id="id"), on="id"),
    "join_semi": lambda f: f.join(
        ur.from_arrow(pa.table({"id": [3, 9]}), id="id"), on="id", how="semi"
    ),
    "as_right": lambda f: ur.from_arrow(pa.table({"id": [3, 4, 999]}), id="id").join(
        f, on="id", how="left"
    ),
}

VERBS: dict = {
    "degree": lambda e: ur.degree(e),
    "pagerank": lambda e: ur.pagerank(e),
    "components": lambda e: ur.connected_components(e),
    "neighbors": lambda e: ur.neighbors(e).agg(col("w").sum()),
}


def cases() -> dict:
    out: dict = {}
    for sname, source in sources().items():
        for rname, (reshape, after) in RESHAPES.items():
            for stat, fn in STATS.items():
                out[f"stat|{sname}|{rname}|{stat}"] = (
                    lambda source=source, reshape=reshape, after=after, fn=fn: fn(
                        after(reshape(source()))
                    )
                )
        for fname, frame in OTHER_FRAMES.items():
            for stat, fn in STATS.items():
                out[f"other|{sname}|{fname}|{stat}"] = lambda source=source, frame=frame, fn=fn: fn(
                    frame(source())
                )
        for bname, fn in BAD_ARGS.items():
            out[f"bad|{sname}|{bname}"] = lambda source=source, fn=fn: fn(source())
        for step, fn in FRAME_STEPS.items():
            out[f"nodes|{sname}|{step}"] = lambda source=source, fn=fn: fn(source().nodes())
            out[f"nodes_filtered|{sname}|{step}"] = lambda source=source, fn=fn: fn(
                source().filter(col("w") > 1.0).nodes()
            )
            out[f"nodes_reshaped|{sname}|{step}"] = lambda source=source, fn=fn: fn(
                source().head(30).nodes()
            )
            for vname, verb in VERBS.items():
                out[f"verb|{sname}|{vname}|{step}"] = lambda source=source, fn=fn, verb=verb: fn(
                    verb(source())
                )
    return out


def masked(rname: str) -> bool:
    """Whether a reshape leaves a filter above the topology: a subgraph view."""
    return "filter" in rname and rname != "filter_head"


def run_all(path: str) -> None:
    with open(path, "w") as f:
        for name, build in cases().items():
            f.write(json.dumps(_record(name, build), sort_keys=True) + "\n")
        # The independent references for the branch's masked stats.
        for sname, source in sources().items():
            for rname, (reshape, after) in RESHAPES.items():
                if not masked(rname):
                    continue

                def view(source=source, reshape=reshape, after=after):
                    return after(reshape(source()))

                def reingest(view=view):
                    t = view().collect().to_arrow()
                    return ur.from_arrow(t, src="s", dst="d")

                for stat in ["apl", "apl_in", "apl_both", "diam_exact", "diam_exact_both"]:
                    rec = _record(
                        f"ref|{sname}|{rname}|{stat}", lambda s=stat: STATS[s](reingest())
                    )
                    f.write(json.dumps(rec, sort_keys=True) + "\n")
                for d in ["out", "in", "both"]:
                    rec = _record(
                        f"refdensity|{sname}|{rname}|{d}",
                        lambda d=d, view=view: (
                            ur.describe(view(), direction=d).collect().to_dicts()[0]["density"]
                        ),
                    )
                    f.write(json.dumps(rec, sort_keys=True) + "\n")
        for sname, source in sources().items():
            for fname in ["hop", "hop_both", "hop_of_filter"]:

                def induced(source=source, fname=fname):
                    e = source()
                    hop = OTHER_FRAMES[fname](e)
                    seeds = {"hop": [0, 3, 21], "hop_both": [5, 6], "hop_of_filter": [0, 3]}[fname]
                    reached = pa.concat_arrays(
                        [
                            pa.array(seeds, pa.int64()),
                            hop.collect().to_arrow().column("dst").combine_chunks(),
                        ]
                    )
                    t = EDGES if fname != "hop_of_filter" else EDGES.filter(pc.field("w") > 1.0)
                    keep = pc.field("s").isin(reached) & pc.field("d").isin(reached)
                    return ur.from_arrow(t.filter(keep), src="s", dst="d")

                for stat in ["apl", "apl_both", "diam_exact", "diam_exact_both"]:
                    rec = _record(
                        f"refhop|{sname}|{fname}|{stat}", lambda s=stat, i=induced: STATS[s](i())
                    )
                    f.write(json.dumps(rec, sort_keys=True) + "\n")


TRAVERSALS = ("hop", "hop_both", "hop_of_filter", "shortest_path")


def category(name: str, a: dict, b: dict) -> str:
    """Why the branch (b) differs from main (a) on a case."""
    kind, _source, what = name.split("|")[:3]
    m = a.get("message", "")
    if kind == "stat" and masked(what) and "error" not in a:
        return "fixed (#210): a stat over a filtered edge frame reads its mask"
    if kind == "other" and what in TRAVERSALS and "error" not in a:
        return "fixed (#210): a stat over a traversal reads its induced subgraph"
    if kind == "other" and what == "verb" and "has no attribute '_node'" in m:
        return "changed (#213): a stat over a standalone verb reads it as a node frame"
    if kind in ("nodes", "nodes_filtered", "nodes_reshaped") and "bare edges.nodes()" in m:
        return "lifted (#213): a bare edges.nodes() collects to its node set"
    if kind == "verb" and "'GraphExpr' object has no attribute" in m:
        return "lifted (#213): a standalone verb has every frame method"
    if kind == "verb" and "join(other): other must be an ursa frame" in m:
        return "lifted (#213): a standalone verb is a join's right side"
    return "UNEXPLAINED"


def _load(path: str) -> dict:
    return {k: {f: v for f, v in r.items() if f != "case"} for k, r in load(path).items()}


def compare(main_path: str, branch_path: str) -> int:
    a, b = _load(main_path), _load(branch_path)
    keys = [k for k in a if not k.startswith("ref")]
    cats: collections.Counter = collections.Counter()
    unexplained = []
    for k in keys:
        if a[k] == b[k]:
            cats["identical"] += 1
            continue
        cat = category(k, a[k], b[k])
        cats[cat] += 1
        if cat == "UNEXPLAINED":
            unexplained.append(k)
    print(f"main vs branch: {len(keys)} cases")
    for cat, n in sorted(cats.items()):
        print(f"  {n:5d}  {cat}")
    for k in unexplained[:30]:
        print("    UNEXPLAINED:", k, "\n      main:  ", a[k], "\n      branch:", b[k])
    # The branch's masked stats against their references.
    checked, wrong = 0, []
    for k in b:
        kind = k.split("|")[0]
        if kind == "ref":
            got = b[k.replace("ref|", "stat|", 1)]
        elif kind == "refhop":
            got = b[k.replace("refhop|", "other|", 1)]
        elif kind == "refdensity":
            _, s, r, d = k.split("|")
            got = b[f"stat|{s}|{r}|{'density' if d == 'out' else 'density_' + d}"]
        else:
            continue
        checked += 1
        same = got == b[k]
        if not same and "float" in got and "float" in b[k]:
            # A mean summed in another order: equal to 1e-12.
            x, y = (struct.unpack("<d", bytes.fromhex(r["float"]))[0] for r in (got, b[k]))
            same = abs(x - y) <= 1e-12 * max(1.0, abs(y))
        if not same:
            wrong.append((k, got, b[k]))
    print(f"branch masked stats vs references: {checked - len(wrong)}/{checked} equal")
    for k, got, ref in wrong[:20]:
        print("    WRONG:", k, got, ref)
    return len(unexplained) + len(wrong)


if __name__ == "__main__":
    if sys.argv[1] == "run":
        run_all(sys.argv[2])
    else:
        sys.exit(1 if compare(sys.argv[2], sys.argv[3]) else 0)
