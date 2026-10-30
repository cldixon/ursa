"""Long chains never crash the process (#216).

A plan nests one level per step. Before #216, cloning, dropping or walking a
long chain recursed once per step: ``ur.degree`` over 20,000 filters
segfaulted on the default stack, and a 20,000-step collect segfaulted on a
256 KB thread stack. Each case runs in a subprocess, so a crash fails the test
rather than the test run, on a thread with a 256 KB stack (Python's smallest
choices are 32 KiB and up; 256 KB is a common embedded default) as well as on
the main thread.
"""

from __future__ import annotations

import subprocess
import sys
import tempfile
import textwrap

import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(not ur._NATIVE_AVAILABLE, reason="native extension not built")

STEPS = 20_000

PRELUDE = textwrap.dedent(
    f"""
    import sys, threading
    import pyarrow as pa, pyarrow.parquet as pq
    import ursa as ur

    STEPS = {STEPS}
    TABLE = pa.table({{"s": [1, 2, 3, 3], "d": [2, 3, 1, 4], "w": [1.0, 2.0, 3.0, 4.0]}})
    mem = ur.EdgeFrame(TABLE, src="s", dst="d")

    def filters(frame, n=STEPS, column="w"):
        for _ in range(n):
            frame = frame.filter(ur.col(column) > -1)
        return frame

    def run_on(stack, body):
        if stack is None:
            body()
            return
        threading.stack_size(stack)
        failure = []
        def target():
            try:
                body()
            except BaseException as e:
                failure.append(e)
        t = threading.Thread(target=target)
        t.start()
        t.join()
        if failure:
            raise failure[0]
    """
)


def _run(body: str, stack: int | None) -> str:
    """Run ``body`` (defining ``main()``) in a fresh interpreter, on a thread
    with ``stack`` bytes of stack (``None``: the main thread), and return what it
    printed. A crash shows as a negative return code."""
    code = PRELUDE + textwrap.dedent(body) + f"\nrun_on({stack!r}, main)\n"
    with tempfile.TemporaryDirectory() as cwd:  # the scans write their files here
        done = subprocess.run(
            [sys.executable, "-c", code], capture_output=True, text=True, timeout=600, cwd=cwd
        )
    assert done.returncode == 0, (done.returncode, done.stderr[-2000:])
    return done.stdout


SMALL = 256 * 1024


@pytest.mark.parametrize("stack", [None, SMALL], ids=["main", "256KB"])
def test_degree_over_a_long_chain_of_filters(stack):
    out = _run(
        """
        def main():
            chain = filters(mem)
            got = ur.degree(chain).collect().to_arrow()
            assert got.equals(ur.degree(mem).collect().to_arrow())
            weighted = ur.pagerank(chain, weight=ur.col("w")).collect().to_arrow()
            assert weighted.equals(ur.pagerank(mem, weight=ur.col("w")).collect().to_arrow())
            print("ok")
        """,
        stack,
    )
    assert out == "ok\n"


@pytest.mark.parametrize("stack", [None, SMALL], ids=["main", "256KB"])
def test_collect_schema_and_explain_of_a_long_chain(stack):
    out = _run(
        """
        def main():
            for source in (mem, ur.scan_edges("edges.parquet", src="s", dst="d")):
                chain = filters(source)
                got = chain.collect().to_arrow()
                assert got.equals(TABLE), got
                assert chain.schema() == got.schema
                assert chain.explain().count("filter(") == STEPS
                del chain
            print("ok")

        pq.write_table(TABLE, "edges.parquet")
        """,
        stack,
    )
    assert out == "ok\n"


@pytest.mark.parametrize("stack", [None, SMALL], ids=["main", "256KB"])
def test_long_chains_of_other_shapes(stack):
    # Node-frame steps after a graph column lower to one DataFusion operator
    # each, which DataFusion's optimizer walks recursively; a reshape, a
    # traversal, a join and samples (which call back into Python) as well.
    out = _run(
        """
        def main():
            chain = filters(mem)
            nodes = chain.nodes().with_columns(d=ur.degree(chain))
            nodes = filters(nodes, n=2_000, column="d")
            got = nodes.collect().to_arrow()
            assert got.equals(mem.nodes().with_columns(d=ur.degree(mem)).collect().to_arrow())
            assert nodes.schema() == got.schema
            assert "DataFusion logical plan" in nodes.explain()

            reshaped = ur.degree(chain.distinct()).collect().to_arrow()
            assert reshaped.equals(ur.degree(mem.distinct()).collect().to_arrow())
            assert ur.hop(chain, 2).from_([1]).collect().to_arrow().num_rows > 0
            joined = chain.join(chain, on=["s", "d", "w"]).collect().to_arrow()
            assert joined.num_rows == 4
            sampled = mem
            for i in range(2_000):
                sampled = sampled.sample(3, seed=i)
            assert sampled.collect().to_arrow().num_rows == 3
            assert ur.describe(chain).collect().to_arrow().num_rows == 1
            print("ok")
        """,
        stack,
    )
    assert out == "ok\n"


def test_a_long_plan_serializes_on_a_small_stack():
    # Plan serialization recurses once per step, on a stack that grows onto
    # the heap (parsing is covered in Rust, `plan::tests`).
    out = _run(
        """
        def main():
            text, slots = filters(mem)._node.lowered()
            # Python's own JSON parser would recurse 20,000 deep: count instead.
            print(text.count('"op":"filter"'))
        """,
        SMALL,
    )
    assert out == f"{STEPS}\n"


def test_a_plan_past_the_stack_cap_raises_ursa_error():
    # The stack a plan may reserve is capped (1 GiB, about 65,000 relational
    # steps); past it the plan raises instead of reserving more.
    out = _run(
        """
        def main():
            chain = filters(mem, n=70_000)
            for run in (chain.collect, chain.schema, chain.explain):
                try:
                    run()
                except ur.UrsaError as e:
                    assert "70000 relational steps" in str(e), e
                    assert "Collect part of the chain" in str(e), e
                else:
                    raise AssertionError("no error")
            short = filters(mem, n=100)
            assert short.collect().to_arrow().equals(TABLE)
            print("ok")
        """,
        SMALL,
    )
    assert out == "ok\n"
