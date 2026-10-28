"""Side-by-side parity harness for #191 part 3 (graph ops over reshaped edge
sets): run every algorithm family over edge frames after each reshaping step,
from memory and from a scan, plus several edge frames in one `with_columns`.

    python parity_191_reshaped.py run OUT.jsonl     # with each build's interpreter
    python parity_191_reshaped.py compare MAIN.jsonl BRANCH.jsonl
    python parity_191_reshaped.py same A.jsonl B.jsonl   # two runs of one build

Each case is recorded twice by `run`: as written, and as its re-ingest
reference (`ref|...`): the reshaped edges collected and built into a new frame
with `ur.from_arrow(...)`, with any steps written after the reshape (a filter, a
column step) applied to that frame. For a mixed `with_columns` the reference is
the same columns, one edge frame per `with_columns` step. Each record is the
result (schema with nullability and metadata, rows with exact float bits) or the
error (class and message), as in `parity_191`.

`compare` checks, on the branch, that every case equals its reference, and, for
each case main ran, that the branch matches main or differs in a known category.
`same` checks two runs of one build are identical (run them under `taskset -c 0`,
`-c 0-2` and the whole machine).
"""

from __future__ import annotations

import collections
import json
import sys
import warnings
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq

sys.path.insert(0, str(Path(__file__).resolve().parent))
from parity_191 import TMP, encode, load

import ursa as ur

warnings.simplefilter("ignore")
col = ur.col

N = 48
_S = [(i * 7 + 3) % N for i in range(160)] + [1, 1, 2, 5, 5, 5]
_D = [(i * 11 + 5) % (N + 6) for i in range(160)] + [2, 2, 1, 6, 6, 6]
EDGES = pa.Table.from_batches(
    pa.table(
        {
            "s": _S,
            "d": _D,
            "w": [round(0.25 + ((i * 37) % 23) / 7, 3) for i in range(len(_S))],
            "k": [i % 5 for i in range(len(_S))],
            "tag": [("abc"[i % 3]) for i in range(len(_S))],
        }
    ).to_batches(max_chunksize=17)
)
pq.write_table(EDGES, TMP / "reshaped.parquet", row_group_size=23)
ALLOWED = pa.table({"s": [0, 3, 7, 10, 17, 24, 31, 44, 47], "grp": list("abcabcabc")})
NODES = pa.table({"id": list(range(N + 8))[::-1], "name": [f"n{i}" for i in range(N + 8)]})


def sources() -> dict:
    return {
        "mem": lambda: ur.from_arrow(EDGES, src="s", dst="d"),
        "scan": lambda: ur.scan_edges(str(TMP / "reshaped.parquet"), src="s", dst="d"),
    }


def _allowed():
    return ur.from_arrow(ALLOWED, id="s")


# name -> (the reshape, the steps written after it). The reference re-ingests the
# reshape's rows and applies the same steps to the new frame.
RESHAPES: dict = {
    "none": (lambda e: e, lambda e: e),
    "distinct": (lambda e: e.select("s", "d").distinct(), lambda e: e),
    "distinct_rows": (lambda e: e.distinct(), lambda e: e),
    "sample": (lambda e: e.sample(90, seed=4), lambda e: e),
    "sample_unseeded": (lambda e: e.sample(70), lambda e: e),
    "semi": (lambda e: e.join(_allowed(), on="s", how="semi"), lambda e: e),
    "anti": (lambda e: e.join(_allowed(), on="s", how="anti"), lambda e: e),
    "inner_join": (lambda e: e.join(_allowed(), on="s"), lambda e: e),
    "left_join": (lambda e: e.join(_allowed(), on="s", how="left"), lambda e: e),
    "group_sum": (
        lambda e: e.group_by("s", "d").agg(w=col("w").sum(), n=col("w").count()),
        lambda e: e,
    ),
    "group_mean": (lambda e: e.group_by("d", "s").agg(w=col("w").mean()), lambda e: e),
    "sort": (lambda e: e.sort("w"), lambda e: e),
    "sort_desc": (lambda e: e.sort("w", descending=True), lambda e: e),
    "sort_keys": (lambda e: e.sort("d", "s"), lambda e: e),
    "head": (lambda e: e.head(70), lambda e: e),
    "sort_head": (lambda e: e.sort("k", "w").head(60), lambda e: e),
    "filter_head": (lambda e: e.filter(col("w") > 1.0).head(50), lambda e: e),
    "head_filter": (lambda e: e.head(120), lambda e: e.filter(col("w") > 1.0)),
    "distinct_filter": (lambda e: e.distinct(), lambda e: e.filter(col("k") != 2)),
    "group_select": (
        lambda e: e.group_by("s", "d").agg(w=col("w").max()),
        lambda e: e.select("s", "d", "w"),
    ),
    "join_group": (
        lambda e: e.join(_allowed(), on="s").group_by("s", "d", "grp").agg(w=col("w").sum()),
        lambda e: e,
    ),
    "reverse_after": (lambda e: e.head(100), lambda e: e.reverse()),
    "reverse_before": (lambda e: e.reverse().sample(80, seed=1), lambda e: e),
    "with_columns": (lambda e: e, lambda e: e.with_columns(w=col("w") * 2 + 1)),
    "with_columns_head": (lambda e: e.with_columns(w=col("w") + col("k")).head(90), lambda e: e),
    "rename_other": (lambda e: e, lambda e: e.rename({"w": "w0"}).rename({"w0": "w"})),
    "select": (lambda e: e, lambda e: e.select("s", "d", "w")),
    "select_filter": (lambda e: e, lambda e: e.select("d", "s", "w").filter(col("w") < 2.0)),
    # Errors: a step that leaves no src / dst column.
    "drop_dst": (lambda e: e, lambda e: e.select("s", "w")),
    "rename_src": (lambda e: e, lambda e: e.rename({"s": "a"})),
    "group_src": (lambda e: e.group_by("s").agg(w=col("w").sum()), lambda e: e),
}

ALGOS: dict = {
    "degree": lambda e: ur.degree(e),
    "degree_both": lambda e: ur.degree(e, "both"),
    "pagerank": lambda e: ur.pagerank(e),
    "pagerank_w": lambda e: ur.pagerank(e, weight=col("w")),
    "pagerank_both": lambda e: ur.pagerank(e, direction="both"),
    "pagerank_both_w": lambda e: ur.pagerank(e, direction="both", weight=col("w")),
    "components": lambda e: ur.connected_components(e),
    "components_strong": lambda e: ur.connected_components(e, mode="strong"),
    "triangles": lambda e: ur.triangle_count(e),
    "louvain": lambda e: ur.louvain(e, seed=11),
    "louvain_w": lambda e: ur.louvain(e, weight=col("w"), seed=11),
    "label_prop": lambda e: ur.label_propagation(e, seed=3),
    "betweenness": lambda e: ur.betweenness(e, sample=0.5, seed=2),
    "closeness": lambda e: ur.closeness(e),
    "closeness_w": lambda e: ur.closeness(e, weight=col("w")),
    "hop": lambda e: ur.hop(e, 2).from_([0, 3, 21]),
    "hop_both": lambda e: ur.hop(e, 1, "both").from_([5]),
    "shortest_path": lambda e: ur.shortest_path(e, 3, 26),
    "shortest_path_w": lambda e: ur.shortest_path(e, 3, 26, weight=col("w")),
    "random_walk": lambda e: ur.random_walk(e, start=[0, 1, 2], steps=5, seed=9),
    "describe": lambda e: ur.describe(e),
    "degree_of_hop": lambda e: ur.degree(ur.hop(e, 2).from_([0, 3])),
    "nodes_tail": lambda e: ur.pagerank(e).filter(col("pagerank") > 0.01).sort("pagerank"),
}


def _reingest(frame):
    return ur.from_arrow(frame.collect().to_arrow(), src=frame.src_col, dst=frame.dst_col)


def cases() -> dict:
    """name -> (the case, its reference)."""
    out: dict = {}
    for sname, source in sources().items():
        for rname, (reshape, after) in RESHAPES.items():
            for aname, algo in ALGOS.items():
                out[f"{sname}|{rname}|{aname}"] = (
                    lambda source=source, reshape=reshape, after=after, algo=algo: algo(
                        after(reshape(source()))
                    ),
                    lambda source=source, reshape=reshape, after=after, algo=algo: algo(
                        after(_reingest(reshape(source())))
                    ),
                )
        # Several edge frames in one with_columns, and its reference: one edge
        # frame per with_columns step.
        mixes = {
            "views": lambda e: [
                ("a", ur.degree(e)),
                ("b", ur.pagerank(e.filter(col("w") > 1.0))),
                ("c", ur.degree(e.filter(col("k") == 1), "in")),
            ],
            "reshaped": lambda e: [
                ("a", ur.pagerank(e)),
                ("b", ur.degree(e.distinct())),
                (
                    "c",
                    ur.pagerank(e.group_by("s", "d").agg(w=col("w").sum()), weight=col("w")),
                ),
                ("d", ur.connected_components(e.head(40))),
            ],
            "same_frame_twice": lambda e: [("a", ur.degree(e)), ("b", ur.pagerank(e))],
        }
        for mname, mix in mixes.items():
            for host in ["attr", "nodes"]:

                def case(source=source, mix=mix, host=host):
                    e = source()
                    base = ur.from_arrow(NODES, id="id") if host == "attr" else e.nodes()
                    return base.with_columns(**dict(mix(e)), z=col("id") + 1)

                def ref(source=source, mix=mix, host=host):
                    e = source()
                    f = ur.from_arrow(NODES, id="id") if host == "attr" else e.nodes()
                    for name, expr in mix(e):
                        f = f.with_columns(**{name: expr})
                    return f.with_columns(z=col("id") + 1)

                out[f"{sname}|mixed|{mname}|{host}"] = (case, ref)
    return out


def _record(name: str, build) -> dict:
    try:
        return {"case": name, **encode(build().collect().to_arrow())}
    except Exception as exc:
        return {"case": name, "error": type(exc).__name__, "message": str(exc)}


def run_all(path: str) -> None:
    with open(path, "w") as out:
        for name, (build, ref) in cases().items():
            out.write(json.dumps(_record(name, build), sort_keys=True) + "\n")
            out.write(json.dumps(_record(f"ref|{name}", ref), sort_keys=True) + "\n")


def category(name: str, a: dict, b: dict) -> str:
    """Why the branch (b) differs from main (a) on a case."""
    rname = name.split("|")[1]
    m = a.get("message", "")
    if "error" in a:
        if "distinct/sample/join/group_by" in m:
            return "lifted: a graph op over a reshaped edge set (main raised)"
        if "same edges frame" in m:
            return "lifted: several edge frames in one with_columns (main raised)"
        if "before nodes()" in m:
            return "lifted: a reshaping step before nodes() (main raised)"
        if "'with_columns' step on an edge frame" in m or (
            "with_columns() takes ursa expressions" in m and rname.startswith("with_columns")
        ):
            return "lifted: with_columns on an edge frame (main raised)"
        if "unknown edge column" in m and rname.startswith(("with_columns", "rename")):
            return "lifted: a weight over a column a column step made (main raised)"
        if "filter() references unknown column" in m and rname.startswith("with_columns"):
            return "lifted: a filter over a column a column step made (main raised)"
        return "UNEXPLAINED"
    if "error" in b and b["error"] == "ColumnNotFoundError" and "a graph op needs" in b["message"]:
        return "changed: a step that drops src/dst now raises (main ignored it)"
    if rname in ("sort", "sort_desc", "sort_keys", "head", "sort_head", "filter_head"):
        return "fixed: sort/head on the edges was ignored by a graph op (main)"
    if rname in ("head_filter", "reverse_after", "with_columns_head"):
        return "fixed: sort/head on the edges was ignored by a graph op (main)"
    if rname in ("select", "select_filter", "group_select") and "unknown edge column" in b.get(
        "message", ""
    ):
        return "fixed: a weight read a column select() had dropped (main)"
    if rname == "with_columns":
        return "fixed: a weight/filter read the column a with_columns replaced (main)"
    return "UNEXPLAINED"


def _load(path: str) -> dict:
    """Records by case, without their own name (so a case and its reference compare)."""
    return {k: {f: v for f, v in r.items() if f != "case"} for k, r in load(path).items()}


def compare(main_path: str, branch_path: str) -> None:
    a, b = _load(main_path), _load(branch_path)
    # 1. The branch against its re-ingest reference.
    ref_cats: collections.Counter = collections.Counter()
    bad_ref = []
    for k in b:
        if k.startswith("ref|"):
            continue
        r = b[f"ref|{k}"]
        if b[k] == r:
            ref_cats["identical to the re-ingest reference"] += 1
        elif "same_frame_twice" in k and b[k] == a[k]:
            # One edge frame: main's own one-call path (the chained reference
            # attaches the second column, so marks it nullable).
            ref_cats["one edge frame: identical to main's one-call result"] += 1
        elif "error" in b[k] and "a graph op needs" in b[k].get("message", ""):
            # The re-ingest can't name the endpoint the step removed: a fresh
            # frame over those rows fails to build (KeyError).
            ref_cats["src/dst dropped: the branch raises, the re-ingest can't build"] += 1
        else:
            bad_ref.append(k)
    print(f"branch vs re-ingest: {sum(ref_cats.values()) + len(bad_ref)} cases")
    for cat, n in sorted(ref_cats.items()):
        print(f"  {n:5d}  {cat}")
    print(f"  {len(bad_ref):5d}  UNEXPLAINED")
    for k in bad_ref[:20]:
        print("    ", k, str(b[k])[:200], "| ref:", str(b[f"ref|{k}"])[:200])
    # 2. Main against the branch, for each case as written.
    cats: collections.Counter = collections.Counter()
    unexplained = []
    keys = [k for k in a if not k.startswith("ref|")]
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
    for k in unexplained[:20]:
        print("    UNEXPLAINED:", k, str(a[k])[:160], "|", str(b[k])[:160])
    # 3. Where main ran the separate collects a mixed with_columns replaces.
    same = sum(
        1 for k in keys if "|mixed|" in k and "error" not in a[f"ref|{k}"] and a[f"ref|{k}"] == b[k]
    )
    ran = sum(1 for k in keys if "|mixed|" in k and "error" not in a[f"ref|{k}"])
    print(f"mixed with_columns vs main's separate with_columns steps: {same}/{ran} identical")


def same(path_a: str, path_b: str) -> None:
    a, b = _load(path_a), _load(path_b)
    diff = [k for k in a if a[k] != b[k]]
    print(f"{len(a)} records, {len(a) - len(diff)} identical")
    for k in diff[:20]:
        print("  DIFFERENT:", k)


if __name__ == "__main__":
    {
        "run": lambda: run_all(sys.argv[2]),
        "compare": lambda: compare(*sys.argv[2:4]),
        "same": lambda: same(*sys.argv[2:4]),
    }[sys.argv[1]]()
