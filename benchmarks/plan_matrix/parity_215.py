"""Parity for #215 (one float semantics for keys): sort every difference between
two builds' `parity_191*.py` results into the one expected category, or flag it.

On the branch, `group_by` keys, `distinct`, `n_unique` and join keys compare
floats in total order: `-0.0 == 0.0` and every NaN equal. A case may differ from
main only because of that. The check is an oracle: main's build run over the
same inputs with every float made canonical first (each NaN the positive quiet
NaN, `-0.0` as `0.0`), where bit-wise and total-order equality agree. With every
float of both results made canonical the same way (a group shows its first
row's value, which may be `-0.0` or another NaN), the branch's result must equal
that one exactly: the same rows, groups, matches, counts, order and schema.

    # with main's interpreter, the harness over canonical inputs:
    python parity_215.py canon parity_191.py OUT.jsonl
    # then, with any interpreter:
    python parity_215.py compare MAIN.jsonl BRANCH.jsonl MAIN_CANON.jsonl
"""

from __future__ import annotations

import collections
import json
import re
import runpy
import struct
import sys
from pathlib import Path

FLOAT_TYPES = ("double", "float", "halffloat")


def canon_inputs() -> None:
    """Make every float of every table entering Ursa (`from_arrow`, a Parquet
    file a harness writes) canonical."""
    import pyarrow as pa
    import pyarrow.compute as pc
    import pyarrow.parquet as pq

    import ursa as ur

    def array(a):
        t = a.type
        if pa.types.is_dictionary(t) and pa.types.is_floating(t.value_type):
            return pa.DictionaryArray.from_arrays(a.indices, array(a.dictionary))
        if not pa.types.is_floating(t):
            return a
        nan = pa.scalar(float("nan"), t)
        return pc.if_else(pc.is_nan(a), nan, pc.add(a, pa.scalar(0.0, t)))

    def table(tbl):
        if not isinstance(tbl, pa.Table):
            return tbl
        cols = [pa.chunked_array([array(c) for c in col.chunks], col.type) for col in tbl.columns]
        return pa.Table.from_arrays(cols, schema=tbl.schema)

    from_arrow, write_table = ur.from_arrow, pq.write_table
    ur.from_arrow = lambda tbl, *a, **kw: from_arrow(table(tbl), *a, **kw)
    pq.write_table = lambda tbl, *a, **kw: write_table(table(tbl), *a, **kw)


def float_columns(schema: str) -> set[str]:
    out = set()
    for line in schema.splitlines():
        m = re.match(r"^(\S+): (\w+)(<values=(\w+))?", line)
        if m and (m.group(2) in FLOAT_TYPES or m.group(4) in FLOAT_TYPES):
            out.add(m.group(1))
    return out


def canon_hex(v: str | None) -> str | None:
    if v is None:
        return None
    raw = bytes.fromhex(v)
    fmt = "<d" if len(raw) == 8 else "<f"
    x = struct.unpack(fmt, raw)[0]
    x = float("nan") if x != x else x + 0.0
    return struct.pack(fmt, x).hex()


def canon(rec: dict) -> dict:
    """`rec` with every float value canonical (and how it ran dropped)."""
    rec = {k: v for k, v in rec.items() if k not in ("stage", "schema_api")}
    if "cols" not in rec:
        return rec
    floats = float_columns(rec["schema"])
    cols = {
        name: [canon_hex(v) for v in vals] if name in floats else vals
        for name, vals in rec["cols"].items()
    }
    return {**rec, "cols": cols}


def load(path: str) -> dict:
    return {json.loads(line)["case"]: json.loads(line) for line in open(path)}


def strip(rec: dict) -> dict:
    """`rec` without how it ran, and with the harness's temporary directory
    masked: each run writes its Parquet files to a fresh one, and an error
    message names the file (#214)."""
    return {
        k: re.sub(r"/tmp/tmp\w+", "TMP", v) if isinstance(v, str) else v
        for k, v in rec.items()
        if k not in ("stage", "schema_api")
    }


def compare(main_path: str, branch_path: str, oracle_path: str) -> int:
    a, b, o = load(main_path), load(branch_path), load(oracle_path)
    cats: collections.Counter = collections.Counter()
    unexplained = []
    for k, x in a.items():
        y = b.get(k)
        if y is not None and strip(x) == strip(y):
            cats["identical"] += 1
        elif y is not None and k in o and canon(y) == canon(o[k]):
            cats["#215: float keys compare in total order"] += 1
        else:
            cats["UNEXPLAINED"] += 1
            unexplained.append(k)
    print(f"{len(a)} cases:")
    for cat, n in sorted(cats.items()):
        print(f"  {n:5d}  {cat}")
    for k in unexplained[:20]:
        print("  UNEXPLAINED:", k)
    return len(unexplained)


if __name__ == "__main__":
    if sys.argv[1] == "canon":
        harness, out = sys.argv[2], sys.argv[3]
        canon_inputs()
        sys.argv = [harness, "run", out]
        sys.path.insert(0, str(Path(harness).resolve().parent))
        runpy.run_path(harness, run_name="__main__")
    else:
        sys.exit(1 if compare(*sys.argv[2:5]) else 0)
