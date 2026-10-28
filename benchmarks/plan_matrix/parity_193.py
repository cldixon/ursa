"""Parity across #193 (frames hold Rust plan objects): the shared runner and
comparison for the `parity_191*.py` harnesses.

On the branch a step that cannot run raises where it is added, not at
`collect()`. The runner records where each error was raised (`stage`: "build"
or the collect) and, for a case that collects, whether `frame.schema()` equals
the collected table's schema (field names, types, nullability and metadata).
`compare` then counts a build-time error as identical to main's collect-time
error when the class and message match, and reports any that don't.

    python parity_191.py run OUT.jsonl              # each build's interpreter
    python parity_191.py compare193 MAIN.jsonl BRANCH.jsonl
"""

from __future__ import annotations

import collections
import json
from collections.abc import Callable
from typing import Any

# Keys a record carries about how a case ran, not what it returned.
_META = ("stage", "schema_api")


def run_case(build: Callable[[], Any], encode: Callable[[Any], dict]) -> dict:
    """Build a case's frame, collect it, and encode the result or the error."""
    try:
        frame = build()
    except Exception as exc:
        return {"error": type(exc).__name__, "message": str(exc), "stage": "build"}
    try:
        table = frame.collect().to_arrow()
    except Exception as exc:
        return {"error": type(exc).__name__, "message": str(exc)}
    rec = encode(table)
    try:
        schema = frame.schema()
    except NotImplementedError:
        return rec  # main: no schema()
    except Exception as exc:
        rec["schema_api"] = f"raised {type(exc).__name__}: {exc}"
        return rec
    same = schema.equals(table.schema, check_metadata=True)
    rec["schema_api"] = "equal" if same else f"differs: {schema} vs {table.schema}"
    return rec


def load(path: str) -> dict:
    return {json.loads(line)["case"]: json.loads(line) for line in open(path)}


def result(rec: dict) -> dict:
    return {k: v for k, v in rec.items() if k not in _META}


def compare(main_path: str, branch_path: str) -> int:
    """Print the comparison; return the number of unexplained differences."""
    a, b = load(main_path), load(branch_path)
    cats: collections.Counter = collections.Counter()
    unexplained: list[str] = []
    schema: collections.Counter = collections.Counter()
    for k, x in a.items():
        y = b.get(k)
        if y is None:
            cats["UNEXPLAINED: missing on the branch"] += 1
            unexplained.append(k)
            continue
        if "error" not in y:
            schema["schema() equals the collected schema"] += y.get("schema_api") == "equal"
            if y.get("schema_api") != "equal":
                schema["SCHEMA MISMATCH: " + k + ": " + str(y.get("schema_api"))] += 1
        if result(x) == result(y):
            if y.get("stage") == "build":
                cats["identical: raised at build (same class and message)"] += 1
            else:
                cats["identical"] += 1
            continue
        if "error" in x and "error" in y and y.get("stage") == "build":
            cat = "UNEXPLAINED: raised at build with another error"
        else:
            cat = "UNEXPLAINED"
        cats[cat] += 1
        unexplained.append(k)
    print(f"{len(a)} cases:")
    for cat, n in sorted(cats.items()):
        print(f"  {n:5d}  {cat}")
    for cat, n in sorted(schema.items()):
        print(f"  {n:5d}  {cat}")
    for k in unexplained[:30]:
        print("  UNEXPLAINED:", k, "\n    main:  ", a[k], "\n    branch:", b.get(k))
    bad = sum(1 for c in schema if c.startswith("SCHEMA MISMATCH"))
    return len(unexplained) + bad
