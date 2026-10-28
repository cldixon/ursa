"""Side-by-side parity harness for #191: run every relational step, and every
ordered pair of steps, on plain, scanned and graph-derived frames, and compare two
builds.

    python parity_191.py run OUT.jsonl            # with each build's interpreter
    python parity_191.py compare MAIN.jsonl BRANCH.jsonl

`run` writes one JSON line per case: the result (schema with nullability and
metadata, rows with exact float bits) or the error (class and message). The
frames cover in-memory and scanned (multi-row-group) node and edge frames, an
attribute frame with a graph column, a join, `edges.nodes()`, a hop and a random
walk, over ints, floats with NaN of both signs, -0.0, +-inf and nulls, strings,
dictionary columns, timestamps, float32 and uint8, in multi-batch inputs. Each
`ref:` frame is the same rows as an engine frame, held as a plain frame.

`compare` reports, for each pair of builds, how many cases are identical and
sorts every difference into a known category (anything uncategorized is a bug),
then checks that every engine-frame result on the branch equals the main build's
plain-frame result over the same rows.
"""

from __future__ import annotations

import itertools
import json
import math
import struct
import sys
import tempfile
import warnings
from pathlib import Path

import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq

import ursa as ur

warnings.simplefilter("ignore")
col = ur.col
NEG_NAN = float((np.array([0.0]) / np.array([0.0]))[0])
N = 41


def _table() -> pa.Table:
    f = [1.5, math.nan, -0.0, 0.0, None, NEG_NAN, 2.5, 1.5, 0.1, -1.0, math.inf, -math.inf]
    s = ["b", "a", None, "a", "c", "b", "a", "d", "é", "a"]
    return pa.table(
        {
            "id": pa.array([(i * 7) % N for i in range(N)], pa.int64()),
            "k": pa.array([i % 4 for i in range(N)], pa.int64()),
            "i": pa.array([None if i % 6 == 0 else (i * 5) % 9 - 3 for i in range(N)], pa.int64()),
            "f": pa.array([f[i % len(f)] for i in range(N)], pa.float64()),
            "s": pa.array([s[i % len(s)] for i in range(N)], pa.string()),
            "c": pa.array(
                [["x", "y", None, "z"][i % 4 if i % 5 else 0] for i in range(N)]
            ).dictionary_encode(),
            "ts": pa.array(
                [None if i % 9 == 4 else (i * 13) % 7 for i in range(N)], pa.timestamp("us")
            ),
            "g": pa.array(
                [None if i % 7 == 3 else ((i * 3) % 5) / 4 for i in range(N)], pa.float32()
            ),
            "u": pa.array([(i * 11) % 5 for i in range(N)], pa.uint8()),
        }
    )


T = pa.Table.from_batches(_table().to_batches(max_chunksize=7))
EDGES = pa.table({"src": list(range(N)), "dst": [(i * 3 + 1) % N for i in range(N)]})
ET = T.rename_columns(["s0", *T.column_names[1:]]).append_column(
    "d0", pa.array([(i * 5 + 2) % N for i in range(N)], pa.int64())
)
ET = pa.Table.from_batches(ET.to_batches(max_chunksize=7))
TMP = Path(tempfile.mkdtemp())
pq.write_table(T, TMP / "nodes.parquet", row_group_size=7)
pq.write_table(ET, TMP / "edges.parquet", row_group_size=7)
pq.write_table(EDGES, TMP / "g.parquet", row_group_size=7)
G = ur.from_arrow(EDGES, src="src", dst="dst")
OTHER = pa.table({"id": [(i * 3) % N for i in range(30)] + [5, 5], "w": list(range(32))})


def frames() -> dict:
    return {
        "plain": ur.from_arrow(T, id="id"),
        "scan": ur.scan_nodes(str(TMP / "nodes.parquet"), id="id"),
        "attr_graph": ur.from_arrow(T, id="id").with_columns(dg=ur.degree(G, direction="in")),
        "scan_graph": ur.scan_nodes(str(TMP / "nodes.parquet"), id="id").with_columns(
            pr=ur.pagerank(G)
        ),
        "join": ur.from_arrow(T, id="id").join(ur.from_arrow(OTHER, id="id"), on="id"),
        "edges_plain": ur.from_arrow(ET, src="s0", dst="d0"),
        "edges_scan": ur.scan_edges(str(TMP / "edges.parquet"), src="s0", dst="d0"),
    }


SINGLE = {
    "filter_f": lambda f, c: f.filter(col("f") > 0),
    "filter_i": lambda f, c: f.filter(col("i") >= 0),
    "filter_s": lambda f, c: f.filter(col("s") == "a"),
    "filter_g": lambda f, c: f.filter(col("g") < 0.5),
    "select": lambda f, c: f.select("k", "f", "s", "c"),
    "rename": lambda f, c: f.rename({"f": "ff", "k": "kk"}),
    "swap": lambda f, c: f.rename({"i": "k", "k": "i"}),
    "sort_f": lambda f, c: f.sort("f"),
    "sort_f_d": lambda f, c: f.sort("f", descending=True),
    "sort_g": lambda f, c: f.sort("g"),
    "sort_k": lambda f, c: f.sort("k", descending=True),
    "sort_i": lambda f, c: f.sort("i"),
    "sort_s": lambda f, c: f.sort("s", descending=True),
    "sort_c": lambda f, c: f.sort("c"),
    "sort_ts": lambda f, c: f.sort("ts"),
    "sort_u": lambda f, c: f.sort("u", descending=True),
    "sort_multi": lambda f, c: f.sort(["k", "s", "i"], descending=[True, False, True]),
    "head": lambda f, c: f.head(5),
    "head0": lambda f, c: f.head(0),
    "head_big": lambda f, c: f.head(1000),
    "head_neg": lambda f, c: f.head(-2),
    "distinct": lambda f, c: f.distinct(),
    "sel_distinct": lambda f, c: f.select("k", "c").distinct(),
    "sample": lambda f, c: f.sample(6, seed=1),
    "sample_noseed": lambda f, c: f.sample(6),
    "sample_all": lambda f, c: f.sample(100, seed=2),
    "group_k": lambda f, c: f.group_by("k").agg(
        fs=col("f").sum(),
        fm=col("f").mean(),
        fmin=col("f").min(),
        fmax=col("f").max(),
        gs=col("g").sum(),
        gm=col("g").mean(),
        gmin=col("g").min(),
        isum=col("i").sum(),
        imean=col("i").mean(),
        imin=col("i").min(),
        imax=col("i").max(),
        ic=col("i").count(),
        iu=col("i").n_unique(),
        smin=col("s").min(),
        smax=col("s").max(),
        su=col("s").n_unique(),
        cu=col("c").n_unique(),
        cmin=col("c").min(),
        tmax=col("ts").max(),
        us=col("u").sum(),
        um=col("u").max(),
        fu=col("f").n_unique(),
        gu=col("g").n_unique(),
        fc=col("f").count(),
        gmax=col("g").max(),
        umean=col("u").mean(),
        tmin=col("ts").min(),
    ),
    "group_nonull": lambda f, c: (
        f.filter(col("f") > -10)
        .group_by("k")
        .agg(fs=col("f").sum(), fm=col("f").mean(), fmax=col("f").max())
    ),
    "group_s": lambda f, c: f.group_by("s").agg(n=col("k").count(), m=col("f").max()),
    "group_c": lambda f, c: f.group_by("c").agg(n=col("k").count(), s=col("g").sum()),
    "group_f": lambda f, c: f.group_by("f").agg(n=col("k").count()),
    "group_2": lambda f, c: f.group_by("k", "s").agg(x=col("f").sum()),
}
ERRORS = {
    "e_sort": lambda f, c: f.sort("nope"),
    "e_sort2": lambda f, c: f.sort(["k", "nope"]),
    "e_rename": lambda f, c: f.rename({"nope": "x"}),
    "e_select": lambda f, c: f.select("k", "nope"),
    "e_group": lambda f, c: f.group_by("nope").agg(n=col("k").count()),
    "e_agg": lambda f, c: f.group_by("k").agg(n=col("nope").count()),
    "e_filter": lambda f, c: f.filter(col("nope") > 1),
    "e_filter_type": lambda f, c: f.filter(col("s") > 1),
}
PAIR = [
    "filter_f",
    "filter_s",
    "select",
    "rename",
    "swap",
    "sort_f",
    "sort_k",
    "sort_multi",
    "head",
    "distinct",
    "sel_distinct",
    "sample",
    "group_k",
    "group_s",
]


def encode(t: pa.Table) -> dict:
    cols = {}
    for name, column in zip(t.column_names, t.columns, strict=True):
        typ = column.type
        if pa.types.is_dictionary(typ):
            column = column.cast(typ.value_type)
        vals = column.to_pylist()
        if pa.types.is_floating(column.type):
            fmt = "<d" if column.type == pa.float64() else "<f"
            vals = [None if v is None else struct.pack(fmt, v).hex() for v in vals]
        else:
            vals = [repr(v) for v in vals]
        cols[name] = vals
    return {
        "schema": t.schema.to_string(show_schema_metadata=True, show_field_metadata=True),
        "rows": t.num_rows,
        "cols": cols,
    }


def run(build, frame, c) -> dict:
    try:
        out = build(frame, c).collect().to_arrow()
    except Exception as exc:
        return {"error": type(exc).__name__, "message": str(exc)}
    return encode(out)


def graph_cases() -> dict:
    """Frames whose rows come from the graph itself."""
    nodes = G.nodes().with_columns(deg=ur.degree(G), pr=ur.pagerank(G))
    hop = ur.hop(G, 2).from_([0, 1, 2, 3, 4])
    walk = ur.random_walk(G, start=list(range(10)), steps=3, seed=4)
    steps = {
        "filter": lambda f, c: f.filter(col(c[0]) > 2),
        "sort": lambda f, c: f.sort(c[1]),
        "sort_d": lambda f, c: f.sort(c[1], descending=True),
        "sort2": lambda f, c: f.sort([c[1], c[0]], descending=[True, False]),
        "head": lambda f, c: f.head(4),
        "distinct": lambda f, c: f.select(c[1]).distinct(),
        "sample": lambda f, c: f.sample(5, seed=3),
        "rename": lambda f, c: f.rename({c[0]: "z"}),
        "select": lambda f, c: f.select(c[1]),
        "group": lambda f, c: f.group_by(c[1]).agg(n=col(c[0]).count(), m=col(c[0]).max()),
        "head_neg": lambda f, c: f.head(-1),
    }
    out = {}
    for fname, frame, cs in [
        ("nodes", nodes, ["id", "deg"]),
        ("hop", hop, ["src", "dst"]),
        ("walk", walk, ["walk_id", "node"]),
        ("ref:nodes", as_plain(nodes, "id"), ["id", "deg"]),
        ("ref:hop", as_plain(hop, "edges"), ["src", "dst"]),
        ("ref:walk", as_plain(walk, "node"), ["walk_id", "node"]),
    ]:
        for a in steps:
            out[f"{fname}|{a}"] = (frame, cs, steps[a])
            for b in steps:
                out[f"{fname}|{a}+{b}"] = (
                    frame,
                    cs,
                    lambda f, c, a=a, b=b: steps[b](steps[a](f, c), c),
                )
    return out


def as_plain(frame, kind):
    t = frame.collect().to_arrow()
    if kind == "edges":
        return ur.from_arrow(t, src="src", dst="dst")
    return ur.from_arrow(t, id=kind)


def run_all(path: str) -> None:
    with open(path, "w") as out:
        write_all(out)


def write_all(out) -> None:
    fr = frames()
    ref = {
        "ref:attr_graph": as_plain(fr["attr_graph"], "id"),
        "ref:scan_graph": as_plain(fr["scan_graph"], "id"),
        "ref:join": as_plain(fr["join"], "id"),
    }
    for fname, frame in list(fr.items()) + list(ref.items()):
        steps = dict(SINGLE, **ERRORS)
        cases = {k: v for k, v in steps.items()}
        for a, b in itertools.product(PAIR, PAIR):
            cases[f"{a}+{b}"] = lambda f, c, a=a, b=b: SINGLE[b](SINGLE[a](f, c), c)
        for name, build in cases.items():
            rec = {"case": f"{fname}|{name}", **run(build, frame, None)}
            out.write(json.dumps(rec, sort_keys=True) + "\n")
    for name, (frame, cs, build) in graph_cases().items():
        rec = {"case": name, **run(build, frame, cs)}
        out.write(json.dumps(rec, sort_keys=True) + "\n")


def load(path: str) -> dict:
    return {json.loads(line)["case"]: json.loads(line) for line in open(path)}


PLAIN_FRAMES = ("plain", "scan", "edges_plain", "edges_scan")


def category(x: dict, y: dict, frame: str, steps: list[str]) -> str:
    if "error" in x and "error" in y:
        return "error class/text (engine frame: now the plain frame's)"
    if "error" in x:
        if "head_neg" in steps:
            return "head(-n): 0 rows (engine frame raised OverflowError)"
        return "main raised (distinct after group_by)"
    if "error" in y:
        return "BRANCH RAISES"
    if x["schema"] != y["schema"] and x["cols"] == y["cols"]:
        return "group_by output nullable"
    if any(s.startswith("sample") for s in steps):
        return "sample picks (engine frame: stdlib draw)"
    if any(s in ("sort_f", "sort_f_d") for s in steps):
        return "#201 float sort"
    if any(s.startswith("group") for s in steps):
        return "float sum/mean/min/max: pyarrow's rules"
    if "sel_distinct" in steps:
        return "main bug: duplicate dictionary rows from a second distinct"
    return "UNEXPLAINED"


def compare(main_path: str, branch_path: str) -> None:
    import collections
    import re

    a, b = load(main_path), load(branch_path)
    cats: collections.Counter = collections.Counter()
    for k in a:
        if a[k] == b[k]:
            continue
        frame, case = k.split("|", 1)
        kind = "plain" if frame in PLAIN_FRAMES or frame.startswith("ref:") else "engine"
        cats[(kind, category(a[k], b[k], frame, case.split("+")))] += 1
    same = sum(a[k] == b[k] for k in a)
    print(f"{len(a)} cases, {same} identical, {len(a) - same} different:")
    for (kind, cat), n in sorted(cats.items()):
        print(f"  {n:5d}  {kind:6s} {cat}")
    # Engine frames on the branch vs main's plain frame over the same rows.
    role = re.compile(r"^(id|src|dst|node|z): (\w+) not null$", re.M)
    norm = lambda r: {**r, "schema": role.sub(r"\1: \2", r["schema"])} if "schema" in r else r  # noqa: E731
    strip = lambda r: {k: v for k, v in r.items() if k != "case"}  # noqa: E731
    ref: collections.Counter = collections.Counter()
    for k in b:
        if k.startswith("ref:") or "ref:" + k not in a:
            continue
        x, y = strip(a["ref:" + k]), strip(b[k])
        steps = k.split("|", 1)[1].split("+")
        if x == y:
            ref["identical"] += 1
        elif norm(x) == norm(y):
            ref["identical but the id/src/dst nullability (from_arrow relaxes it)"] += 1
        elif any(s in ("sort_f", "sort_f_d") for s in steps):
            ref["#201 float sort"] += 1
        elif k.startswith("scan_graph|e_"):
            ref["error from the scan (the reference is in memory)"] += 1
        else:
            ref["UNEXPLAINED: " + k] += 1
    print("branch engine frames vs main's plain frame over the same rows:")
    for cat, n in sorted(ref.items()):
        print(f"  {n:5d}  {cat}")


if __name__ == "__main__":
    if sys.argv[1] == "run":
        run_all(sys.argv[2])
    else:
        compare(sys.argv[2], sys.argv[3])
