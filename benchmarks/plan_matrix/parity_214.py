"""Parity across #214 (the error message audit): compare two runs of a
`parity_191*.py` harness and check that only error messages changed.

A case is identical, or *reworded*: both builds raised the same exception class
where the step is added, and only the message differs. Every distinct
(class, old message, new message) is printed, so each rewording is listed. Any
other difference is unexplained. The branch's messages are also checked for the
internals #214 removed (`LEAKS`).

    python parity_191.py run OUT.jsonl              # each build's interpreter
    python parity_214.py MAIN.jsonl BRANCH.jsonl
"""

from __future__ import annotations

import collections
import json
import sys

# Text no user-facing message may contain: DataFusion's internal qualifier,
# Ursa's private column names, serde's wording, and DataFusion's and Arrow's
# variant prefixes.
LEAKS = (
    "?table?",
    "__ursa_",
    "invalid type:",
    "expected a string",
    "at line ",
    "Arrow error:",
    "Error during planning:",
    "Execution error:",
    "This feature is not implemented:",
    "Schema error:",
    "Internal error:",
    "invalid plan:",
    "Cast error",
    "Parquet error",
    "Invalid argument error",
    "Object Store error",
    "External error",
    "caused by",
    "Cannot coerce",
    "Cannot infer common argument",
    "failed to match any signature",
    "No field named",
    "Utf8",
    "Timestamp(",
    "Dictionary(",
    "Decimal128(",
)


def leaks(message: str) -> list[str]:
    """The `LEAKS` that `message` contains."""
    return [t for t in LEAKS if t in message]


def load(path: str) -> dict:
    return {json.loads(line)["case"]: json.loads(line) for line in open(path)}


def compare(main_path: str, branch_path: str) -> int:
    """Print the comparison; return the number of unexplained differences and
    leaking messages."""
    a, b = load(main_path), load(branch_path)
    cats: collections.Counter = collections.Counter()
    pairs: collections.Counter = collections.Counter()
    unexplained: list[str] = []
    leaking: collections.Counter = collections.Counter()
    for k, x in a.items():
        y = b.get(k)
        if y is None:
            cats["UNEXPLAINED: missing on the branch"] += 1
            unexplained.append(k)
            continue
        if "message" in y and leaks(y["message"]):
            leaking[y["message"]] += 1
        if x == y:
            cats["identical"] += 1
            continue
        same_but_message = (
            "error" in x
            and "error" in y
            and x["error"] == y["error"]
            and x.get("stage") == y.get("stage")
            and {k: v for k, v in x.items() if k != "message"}
            == {k: v for k, v in y.items() if k != "message"}
        )
        if same_but_message:
            cats["reworded (same class and stage)"] += 1
            pairs[(x["error"], x["message"], y["message"])] += 1
            continue
        cats["UNEXPLAINED"] += 1
        unexplained.append(k)
    print(f"{len(a)} cases:")
    for cat, n in sorted(cats.items()):
        print(f"  {n:5d}  {cat}")
    print(f"{len(pairs)} distinct rewordings:")
    for (cls, old, new), n in sorted(pairs.items()):
        print(f"  {n:5d}  {cls}\n         main:   {old}\n         branch: {new}")
    for message, n in sorted(leaking.items()):
        print(f"  LEAK ({n}): {message}")
    for k in unexplained[:30]:
        print("  UNEXPLAINED:", k, "\n    main:  ", a[k], "\n    branch:", b.get(k))
    return len(unexplained) + len(leaking)


if __name__ == "__main__":
    sys.exit(1 if compare(sys.argv[1], sys.argv[2]) else 0)
