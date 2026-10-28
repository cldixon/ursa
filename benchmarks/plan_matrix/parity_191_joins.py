"""Side-by-side parity harness for #191 part 2 (joins in the engine): run joins of
every `how`, before and after each relational step, chained two and three deep,
on plain, scanned and graph-derived frames and traversal results, and compare
two builds.

    python parity_191_joins.py run OUT.jsonl        # with each build's interpreter
    python parity_191_joins.py compare MAIN.jsonl BRANCH.jsonl

The data is `parity_191`'s (ints, floats with NaN of both signs, -0.0, +-inf and
nulls, strings, dictionary columns, ...), in multi-batch inputs and Parquet files
of 7-row groups. The right sides repeat keys (many-to-many), hold null keys and
keys the left side lacks, and are keyed by int, float (NaN, -0.0), string,
dictionary and two-column keys. Each case records the result (schema with
nullability and metadata, rows with exact float bits) or the error (class and
message), as `parity_191` does.

`compare` sorts every difference into a known category: a limit main raised for
(a `how` it lacked, a second join, a join on a traversal result or after a
random walk / describe) or the reworded message for an unknown `how`. Anything
else is a regression. Run `run` under `taskset -c 0`, `-c 0-2` and the whole
machine, and `compare` the branch's files pairwise, to check that the order
holds on every partition count.
"""

from __future__ import annotations

import json
import sys
import warnings
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

sys.path.insert(0, str(Path(__file__).resolve().parent))
from parity_191 import EDGES, NEG_NAN, OTHER, TMP, G, T, encode, load

import ursa as ur

warnings.simplefilter("ignore")
col = ur.col
nan = float("nan")

HOWS = ["inner", "left", "right", "outer", "full", "semi", "anti", "cross"]

# Right sides, keyed by "id"; `rkey` renames the key for a left frame with
# another key column.
R_ID = pa.Table.from_batches(
    pa.table(
        {
            "id": [5, 1, None, 7, 1, 3, 3, 40, 50, 0, 14, 14, None, 21],
            "w": [1.5, 2.5, 3.5, nan, 5.5, 6.5, -0.0, 8.5, 9.5, NEG_NAN, 11.5, 12.5, 13.5, None],
            "t": ["a", "b", "c", "d", None, "f", "g", "h", "i", "j", "k", "l", "m", "n"],
        }
    ).to_batches(max_chunksize=4)
)
pq.write_table(R_ID, TMP / "rid.parquet", row_group_size=4)
R_F = pa.table({"f": [nan, NEG_NAN, -0.0, 0.0, 1.5, None, float("inf"), 1.5], "fw": list(range(8))})
R_S = pa.table({"s": ["a", "b", None, "é", "zz", "a"], "sw": list(range(6))})
R_C = pa.table(
    {"c": pa.array(["x", "z", None, "x", "y"]).dictionary_encode(), "cw": list(range(5))}
)
R_CS = pa.table({"c": ["x", "z", None, "y"], "cw": list(range(4))})  # string vs dictionary
R_KS = pa.table(
    {"k": [0, 1, 1, 2, None, 3], "s": ["a", "a", "b", None, "a", "d"], "kw": list(range(6))}
)
R_3 = pa.table({"id": [9, 1, 3, 1, 0, None, 5], "x": ["p", "q", "r", "s", "t", "u", "v"]})
R_4 = pa.table({"id": [1, 9, 2, 0, 5], "y": [True, False, True, None, False]})


def rkey(t: pa.Table, key: str) -> pa.Table:
    return t.rename_columns([key if c == "id" else c for c in t.column_names])


def right_sides(key: str) -> dict:
    """Right frames keyed by `key`: in memory, scanned, and engine frames."""
    nodes = G.nodes().with_columns(deg=ur.degree(G), pr=ur.pagerank(G))
    out = {
        "mem": ur.from_arrow(rkey(R_ID, key), id=key),
        "scan": ur.scan_nodes(str(TMP / "rid.parquet"), id="id").rename({"id": key})
        if key != "id"
        else ur.scan_nodes(str(TMP / "rid.parquet"), id="id"),
        "graph": nodes.rename({"id": key}) if key != "id" else nodes,
        "join": ur.from_arrow(rkey(R_ID, key), id=key).join(
            ur.from_arrow(rkey(R_3, key), id=key), on=key, how="left"
        ),
        "hop": ur.hop(G, 1).from_([0, 1, 2, 5]).rename({"src": key, "dst": "hd"}),
        "group": ur.from_arrow(rkey(R_ID, key), id=key)
        .group_by(key)
        .agg(n=col("w").count(), ws=col("w").sum()),
    }
    return out


def left_frames() -> dict:
    """Left frames and their key column."""
    walk = ur.random_walk(G, start=list(range(8)), steps=3, seed=4)
    return {
        "plain": (ur.from_arrow(T, id="id"), "id"),
        "scan": (ur.scan_nodes(str(TMP / "nodes.parquet"), id="id"), "id"),
        "attr_graph": (
            ur.from_arrow(T, id="id").with_columns(dg=ur.degree(G, direction="in")),
            "id",
        ),
        "nodes": (G.nodes().with_columns(dg=ur.degree(G)), "id"),
        "join": (ur.from_arrow(T, id="id").join(ur.from_arrow(OTHER, id="id"), on="id"), "id"),
        "group": (ur.from_arrow(T, id="id").group_by("id").agg(n=col("k").count()), "id"),
        "edges": (ur.from_arrow(EDGES, src="src", dst="dst"), "src"),
        "edges_scan": (ur.scan_edges(str(TMP / "g.parquet"), src="src", dst="dst"), "src"),
        "hop": (ur.hop(G, 2).from_([0, 1, 2, 3, 4]), "src"),
        "path": (ur.shortest_path(G, source=0, target=7), "src"),
        "walk": (walk, "node"),
    }


def pre_steps(key: str) -> dict:
    """Steps written on the left frame before the join."""
    return {
        "none": lambda f: f,
        "filter": lambda f: f.filter(col(key) > 3),
        "sort": lambda f: f.sort(key, descending=True),
        "head": lambda f: f.head(9),
        "distinct": lambda f: f.select(key).distinct(),
        "sample": lambda f: f.sample(12, seed=5),
        "group": lambda f: f.group_by(key).agg(cnt=col(key).count()),
        "with_columns": lambda f: f.with_columns(k2=col(key) * 2),
        "rename": lambda f: f.select(key).rename({key: key}),
    }


def post_steps(key: str) -> dict:
    """Steps written after the join."""
    return {
        "none": lambda f: f,
        "filter": lambda f: f.filter(col(key) < 20),
        "sort": lambda f: f.sort(key),
        "sort_d": lambda f: f.sort(key, descending=True),
        "head": lambda f: f.head(7),
        "distinct": lambda f: f.select(key).distinct(),
        "sample": lambda f: f.sample(6, seed=3),
        "group": lambda f: f.group_by(key).agg(n=col(key).count()),
        "with_columns": lambda f: f.with_columns(k3=col(key) + 1),
    }


def cases() -> dict:
    out: dict = {}
    for lname, (left, key) in left_frames().items():
        rights = right_sides(key)
        for how in HOWS:
            # Every right side, as is.
            for rname, right in rights.items():
                out[f"{lname}|{rname}|{how}"] = lambda left=left, right=right, how=how, key=key: (
                    left.join(right, on=key, how=how)
                )
            # Steps before and after the join (in-memory right side).
            for pname, pre in pre_steps(key).items():
                for qname, post in post_steps(key).items():
                    # Every pair for the hows main ran; one side bare for the rest.
                    if "none" not in (pname, qname) and how not in ("inner", "left"):
                        continue
                    out[f"{lname}|mem|{how}|{pname}>{qname}"] = (
                        lambda left=left, how=how, key=key, pre=pre, post=post, r=rights["mem"]: (
                            post(pre(left).join(r, on=key, how=how))
                        )
                    )
            # A stepped right side.
            for pname, pre in pre_steps(key).items():
                out[f"{lname}|mem<{pname}|{how}"] = (
                    lambda left=left, how=how, key=key, pre=pre, r=rights["mem"]: left.join(
                        pre(r), on=key, how=how
                    )
                )
        # Two and three joins.
        for h1 in ["inner", "left", "right", "outer"]:
            for h2 in ["inner", "left", "right", "outer", "semi", "anti"]:
                out[f"{lname}|two|{h1}+{h2}"] = lambda left=left, key=key, h1=h1, h2=h2: left.join(
                    ur.from_arrow(rkey(R_ID, key), id=key), on=key, how=h1
                ).join(ur.from_arrow(rkey(R_3, key), id=key), on=key, how=h2)
            out[f"{lname}|three|{h1}"] = lambda left=left, key=key, h1=h1: (
                left.join(ur.from_arrow(rkey(R_ID, key), id=key), on=key, how="left")
                .join(ur.from_arrow(rkey(R_3, key), id=key), on=key, how=h1)
                .join(ur.from_arrow(rkey(R_4, key), id=key), on=key, how="outer")
                .sort(key)
            )
        # A column collision.
        out[f"{lname}|collide"] = lambda left=left, key=key: left.join(left, on=key)
    # Other key types, on the attribute frame (plain and graph-derived).
    for lname, left in [
        ("plain", ur.from_arrow(T, id="id")),
        ("attr_graph", ur.from_arrow(T, id="id").with_columns(dg=ur.degree(G, direction="in"))),
    ]:
        for how in HOWS:
            for kname, (right, on) in {
                "float": (ur.from_arrow(R_F, id="fw"), "f"),
                "string": (ur.from_arrow(R_S, id="sw"), "s"),
                "dict": (ur.from_arrow(R_C, id="cw"), "c"),
                "dict_vs_string": (ur.from_arrow(R_CS, id="cw"), "c"),
                "two_keys": (ur.from_arrow(R_KS, id="kw"), ["k", "s"]),
            }.items():
                out[f"{lname}|{kname}|{how}"] = lambda left=left, right=right, on=on, how=how: (
                    left.join(right, on=on, how=how)
                )
    # A join after describe() and after a random walk's steps.
    out["describe|join"] = lambda: ur.describe(G).join(
        ur.from_arrow(pa.table({"n_nodes": [41, 3], "z": [1, 2]}), id="n_nodes"), on="n_nodes"
    )
    return out


def run_all(path: str) -> None:
    with open(path, "w") as out:
        for name, build in cases().items():
            try:
                rec = {"case": name, **encode(build().collect().to_arrow())}
            except Exception as exc:
                rec = {"case": name, "error": type(exc).__name__, "message": str(exc)}
            out.write(json.dumps(rec, sort_keys=True) + "\n")


def category(x: dict) -> str:
    """Why main differs: the limit it raised for, or a reworded message."""
    m = x.get("message", "")
    if "join how=" in m and "is not supported yet" in m:
        return "lifted: right/outer/semi/anti (main raised)"
    if "a single join()" in m:
        return "lifted: two or more joins (main raised)"
    if "join() on a traversal result" in m:
        return "lifted: join on a traversal result (main raised)"
    if "'join' step after" in m:
        return "lifted: join after describe() / random_walk (main raised)"
    if "'with_columns' step on an edge frame" in m:
        return "lifted: with_columns on an edge frame (#191 part 3; main raised)"
    return "UNEXPLAINED"


def compare(main_path: str, branch_path: str) -> None:
    import collections

    a, b = load(main_path), load(branch_path)
    cats: collections.Counter = collections.Counter()
    unexplained = []
    for k in a:
        if a[k] == b[k]:
            continue
        cat = category(a[k]) if "error" in a[k] else "UNEXPLAINED"
        if "error" in a[k] and "error" in b[k] and "join how=" in a[k]["message"]:
            cat = "unknown how: reworded message (lists the hows)"
        cats[cat] += 1
        if cat == "UNEXPLAINED":
            unexplained.append(k)
    same = sum(a[k] == b[k] for k in a)
    print(f"{len(a)} cases, {same} identical, {len(a) - same} different:")
    for cat, n in sorted(cats.items()):
        print(f"  {n:5d}  {cat}")
    for k in unexplained[:20]:
        print("  UNEXPLAINED:", k)


if __name__ == "__main__":
    if sys.argv[1] == "run":
        run_all(sys.argv[2])
    else:
        compare(sys.argv[2], sys.argv[3])
