"""Frames hold Rust plan objects (#193): validation where a step is added,
``schema()``, ``explain()``, the caches on the Rust state, and the frame
behaviours that stay as they were (types, repr, copy, hashing, threads)."""

from __future__ import annotations

import ast
import copy
import pickle
import struct
import subprocess
import sys
import textwrap
import threading
import time
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa._execute import _native

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

col = ur.col
EDGES = pa.table(
    {"s": [0, 0, 1, 2, 3, 3], "d": [1, 2, 0, 0, 1, 2], "w": [0.5, 1.5, 2.0, 1.0, 3.0, 0.25]}
)
NODES = pa.table({"id": [0, 1, 2, 3], "label": ["a", "b", "c", "d"], "v": [1, 2, 3, 4]})


def _edges() -> ur.EdgeFrame:
    return ur.from_arrow(EDGES, src="s", dst="d")


def _nodes() -> ur.NodeFrame:
    return ur.from_arrow(NODES, id="id")


def _scans(tmp_path) -> tuple[ur.EdgeFrame, ur.NodeFrame]:
    pq.write_table(EDGES, tmp_path / "e.parquet")
    pq.write_table(NODES, tmp_path / "n.parquet")
    return (
        ur.scan_edges(str(tmp_path / "e.parquet"), src="s", dst="d"),
        ur.scan_nodes(str(tmp_path / "n.parquet"), id="id"),
    )


# --- schema() ---------------------------------------------------------------------
def _frames(tmp_path) -> dict:
    e, n = _edges(), _nodes()
    se, sn = _scans(tmp_path)
    hop = ur.hop(e, 2).from_([0])
    return {
        "edges": e,
        "nodes": n,
        "scan_edges": se,
        "scan_nodes": sn,
        "scan_edges_steps": se.filter(col("w") > 1).select("s", "d", (col("w") * 2).alias("w2")),
        "graph_columns": e.nodes().with_columns(
            deg=ur.degree(e), pr=ur.pagerank(e, weight=col("w")), c=ur.connected_components(e)
        ),
        "attribute_graph": n.with_columns(
            deg=ur.degree(e), avg=ur.neighbors(e).agg(col("v").mean()), x=col("v") * 1.5
        ),
        "scan_attribute_graph": sn.with_columns(deg=ur.degree(se), f=ur.pagerank(se, dtype="f32")),
        "reverse": e.reverse(),
        "group_by": n.group_by("label").agg(n=col("v").count(), m=col("v").mean()),
        "distinct": e.select("s").distinct(),
        "sample": e.sample(3, seed=1),
        "sort_head": n.sort("v", descending=True).head(2),
        "rename_select": n.rename({"v": "value"}).select("id", "value"),
        "join_inner": n.join(e.nodes().with_columns(deg=ur.degree(e)), on="id"),
        "join_left": n.join(n.select("id", "v").rename({"v": "v2"}), on="id", how="left"),
        "join_outer": n.join(n.select("id", "v").rename({"v": "v2"}), on="id", how="outer"),
        "join_semi": n.join(hop.rename({"dst": "id"}).select("id"), on="id", how="semi"),
        "hop": hop,
        "hop_degree": ur.degree(hop)._frame(),
        "shortest_path": ur.shortest_path(e, 3, 0, weight=col("w")),
        "random_walk": ur.random_walk(e, start=[0, 1], steps=3, seed=2),
        "describe": ur.describe(e, full=True),
        "describe_tail": ur.describe(e).select("n_nodes", "density"),
        "reshaped": ur.pagerank(e.group_by("s", "d").agg(w=col("w").sum()))._frame(),
        "edge_with_columns": e.with_columns(w2=col("w") + 1),
    }


def test_schema_equals_the_collected_schema_on_every_frame_type(tmp_path):
    for name, frame in _frames(tmp_path).items():
        table = frame.collect().to_arrow()
        schema = frame.schema()
        assert isinstance(schema, pa.Schema), name
        assert schema.equals(table.schema, check_metadata=True), (name, schema, table.schema)


def test_schema_keeps_metadata_and_nullability():
    schema = pa.schema(
        [pa.field("id", pa.int64()), pa.field("x", pa.float64(), nullable=False)],
        metadata={"k": "v"},
    )
    t = pa.table({"id": [0, 1], "x": [1.0, 2.0]}, schema=schema)
    n = ur.from_arrow(t, id="id")
    for frame in (n, n.filter(col("x") > 0), n.select("x", "id")):
        assert frame.schema().equals(frame.collect().to_arrow().schema, check_metadata=True)
        assert not frame.schema().field("x").nullable
        assert frame.schema().metadata == {b"k": b"v"}


def test_schema_runs_nothing(tmp_path):
    e, _ = _scans(tmp_path)
    pipeline = e.nodes().with_columns(pr=ur.pagerank(e)).filter(col("pr") > 0)
    before = _native()._topology_build_count()
    _native()._scan_log(True)
    assert pipeline.schema().names == ["id", "pr"]
    assert "DataFusion" in pipeline.explain()
    assert _native()._scan_log(False) == []
    assert _native()._topology_build_count() == before
    assert e._index_build_cell.value is None


def _corrupt_pages(path: Path) -> None:
    """Overwrite a Parquet file's data pages, keeping its footer readable."""
    raw = bytearray(path.read_bytes())
    footer = struct.unpack("<I", raw[-8:-4])[0]
    end = len(raw) - 8 - footer
    raw[4:end] = b"\xff" * (end - 4)
    path.write_bytes(bytes(raw))


def test_schema_reads_no_rows_of_a_scan(tmp_path):
    e, n = _scans(tmp_path)
    _corrupt_pages(tmp_path / "e.parquet")
    _corrupt_pages(tmp_path / "n.parquet")
    frames = [
        e.filter(col("w") > 1).select("s", "d", "w"),
        e.nodes().with_columns(pr=ur.pagerank(e, weight=col("w"))),
        n.join(e.nodes().with_columns(deg=ur.degree(e)), on="id"),
    ]
    for frame in frames:
        frame.schema()
        frame.explain()
        with pytest.raises(Exception):  # noqa: B017 - whatever the reader raises
            frame.collect()


def _parity_modules():
    import importlib
    import sys

    here = Path(__file__).resolve().parents[1] / "benchmarks" / "plan_matrix"
    sys.path.insert(0, str(here))
    try:
        names = ("parity_191", "parity_191_joins", "parity_191_reshaped")
        return tuple(importlib.import_module(n) for n in names)
    finally:
        sys.path.remove(str(here))


def _parity_cases():
    p191, joins, reshaped = _parity_modules()
    fr = p191.frames()
    for fname, frame in fr.items():
        steps = dict(p191.SINGLE, **p191.ERRORS)
        for sname, build in steps.items():
            yield f"{fname}|{sname}", lambda b=build, f=frame: b(f, None)
        for a in p191.PAIR:
            for b in p191.PAIR:
                yield (
                    f"{fname}|{a}+{b}",
                    lambda a=a, b=b, f=frame: p191.SINGLE[b](p191.SINGLE[a](f, None), None),
                )
    for name, (frame, cs, build) in p191.graph_cases().items():
        yield name, lambda b=build, f=frame, c=cs: b(f, c)
    yield from joins.cases().items()
    for name, (build, _ref) in reshaped.cases().items():
        yield name, build


def test_schema_equals_the_collected_schema_across_the_parity_plans():
    # Every plan of the #191 parity harnesses that collects: its schema() is the
    # collected table's, names, types, nullability and metadata (#193).
    checked = 0
    for name, build in _parity_cases():
        try:
            frame = build()
            table = frame.collect().to_arrow()
        except Exception:
            continue  # a case that raises has no schema to compare
        schema = frame.schema()
        assert schema.equals(table.schema, check_metadata=True), (name, schema, table.schema)
        checked += 1
    assert checked > 7000


# --- explain() --------------------------------------------------------------------
def test_explain_shows_the_plan_and_the_datafusion_plans(tmp_path):
    e = _edges()
    frame = e.nodes().with_columns(pr=ur.pagerank(e)).filter(col("pr") > 0.1).sort("pr")
    text = frame.explain()
    assert text.startswith("NodeFrame plan:")
    for step in ("sort(", "filter(", "with_columns(pr=pagerank(", "nodes()", "from_arrow("):
        assert step in text
    assert "Schema:" in text and "pr: Float64" in text
    assert "DataFusion logical plan 1 of" in text and "GraphAlgorithm" in text
    assert "DataFusion physical plan" not in text
    assert "DataFusion physical plan 1 of" in frame.explain(physical=True)


def test_explain_runs_on_any_frame(tmp_path):
    e = _edges()
    missing = ur.scan_edges(str(tmp_path / "missing.parquet"), src="s", dst="d")
    for frame in (
        e,
        e.nodes(),  # does not collect (no columns yet)
        missing,  # its file does not exist
        ur.hop(e, 1).from_([0]),
        ur.describe(e),
        ur.random_walk(e, start=[0], steps=2),
        ur.pagerank(e),  # a bare algorithm explains as its frame
    ):
        assert "plan:" in frame.explain()
    assert "not available" in e.nodes().explain()


# --- validation where a step is added ---------------------------------------------
def _bad_steps():
    e, n = _edges(), _nodes()
    g = e.nodes().with_columns(pr=ur.pagerank(e))
    hop = ur.hop(e, 1).from_([0])
    return [
        (lambda: n.filter(col("nope") > 1), ur.ColumnNotFoundError, r"filter\(\) references"),
        (lambda: g.filter(col("nope") > 1), ur.ColumnNotFoundError, r"filter\(\) references"),
        (lambda: n.select("nope"), ur.ColumnNotFoundError, r"select\(\) references"),
        (lambda: n.select("id", "id"), ValueError, "duplicate column"),
        (lambda: n.with_columns(x=col("nope") + 1), ur.ColumnNotFoundError, "with_columns"),
        (lambda: n.rename({"nope": "x"}), ur.ColumnNotFoundError, r"rename\(\) references"),
        (lambda: n.sort("nope"), ur.ColumnNotFoundError, r"sort\(\) references"),
        (lambda: n.sort([]), ValueError, "at least one column"),
        (lambda: n.group_by("nope").agg(c=col("v").count()), ur.ColumnNotFoundError, "group_by"),
        (lambda: n.group_by("id").agg(c=col("nope").sum()), ur.ColumnNotFoundError, r"agg\(\)"),
        (lambda: n.join(e.nodes(), on="id"), NotImplementedError, "right-hand frame"),
        (lambda: n.join(n, on="nope"), ur.ColumnNotFoundError, "not in the left frame"),
        (lambda: n.join(n, on="id"), ValueError, "duplicate column"),
        (lambda: n.join(n, on="id", how="cross"), NotImplementedError, "how='cross'"),
        (lambda: n.filter(col("label") > 1), ur.ComputeError, "no kernel matching"),
        (lambda: n.filter(col("v")), NotImplementedError, "boolean predicate"),
        (lambda: n.filter(ur.src() == 1), NotImplementedError, "not available"),
        (lambda: hop.filter(ur.id() == 1), NotImplementedError, "not available"),
        (lambda: hop.reverse(), NotImplementedError, "reverse"),
        (lambda: ur.hop(hop, 1).from_([0]), NotImplementedError, "chaining a traversal"),
        (lambda: ur.degree(hop.head(2))._frame(), NotImplementedError, "relational step"),
        (lambda: ur.degree(e.select("s", "w"))._frame(), ur.ColumnNotFoundError, "dst column"),
        (lambda: ur.pagerank(e, weight=col("nope"))._frame(), ur.ColumnNotFoundError, "weight"),
        (lambda: e.with_columns(pr=ur.pagerank(e)), NotImplementedError, "edge frame"),
        (
            lambda: n.with_columns(deg=ur.degree(e)).select("v").with_columns(d=ur.degree(e)),
            ur.ColumnNotFoundError,
            "node id column",
        ),
        (
            lambda: e.nodes().sort("id").with_columns(d=ur.degree(e)),
            NotImplementedError,
            r"with_columns\(\) after sort",
        ),
        (
            lambda: e.nodes().filter(col("nope") > 1).with_columns(d=ur.degree(e)),
            ur.ColumnNotFoundError,
            r"the frame has \['id', 'd'\]",
        ),
        (
            lambda: ur.describe(e).with_columns(x=col("n_nodes")),
            NotImplementedError,
            "after describe",
        ),
        (
            lambda: ur.random_walk(e, start=[0], steps=2).with_columns(x=col("step")),
            NotImplementedError,
            "after a random_walk",
        ),
    ]


@pytest.mark.parametrize("case", range(len(_bad_steps())))
def test_an_invalid_step_raises_where_it_is_added(case):
    build, exc, match = _bad_steps()[case]
    with pytest.raises(exc, match=match):
        build()


def test_build_time_errors_are_the_ones_collect_raised():
    # The class and message are what collect() raised; the step that adds the
    # problem raises it.
    n = _nodes()
    with pytest.raises(ur.ColumnNotFoundError) as exc:
        n.filter(col("nope") > 1)
    assert str(exc.value) == (
        "filter() references unknown column 'nope'; the frame has ['id', 'label', 'v']."
    )
    assert isinstance(exc.value, ValueError)


def test_what_only_the_rows_can_tell_still_raises_at_collect(tmp_path):
    n = _nodes()
    zero = n.with_columns(q=col("v") / (col("v") - col("v")))  # integer divide by zero
    with pytest.raises(ur.ComputeError, match="Divide by zero"):
        zero.collect()
    nulls = ur.from_arrow(pa.table({"s": [0, None], "d": [1, 2]}), src="s", dst="d")
    degree = ur.degree(nulls)._frame()  # builds: the null is in the rows
    with pytest.raises(ValueError):
        degree.collect()
    # A scan's columns are its file's, read when it runs (it may also change).
    _, sn = _scans(tmp_path)
    typo = sn.filter(col("nope") > 1)
    with pytest.raises(ur.ColumnNotFoundError, match="nope"):
        typo.collect()
    missing = ur.scan_edges(str(tmp_path / "missing.parquet"), src="s", dst="d")
    with pytest.raises(ur.UrsaError):
        ur.degree(missing).collect()
    # A frame that only a later step makes collectable builds.
    bare = _edges().nodes().filter(col("id") > 0)
    with pytest.raises(NotImplementedError, match=r"bare edges\.nodes"):
        bare.collect()
    assert bare.with_columns(d=ur.degree(_edges())).collect().to_arrow().num_rows == 3


def test_a_step_a_graph_op_reads_only_the_topology_of_is_checked_too():
    # These ran before #193, skipping the bad step: a graph op (or nodes()) over
    # the frame read only its topology. The step is checked where it is added.
    e = _edges()
    with pytest.raises(ur.ColumnNotFoundError, match=r"select\(\) references unknown"):
        ur.degree(e.select("s", "d", "nope"))._frame()
    with pytest.raises(ur.ColumnNotFoundError, match=r"filter\(\) references unknown"):
        e.filter(col("nope") > 1).nodes()


def test_a_long_pipeline_validates_each_step():
    f = _nodes()
    for i in range(300):
        f = f.filter(col("v") > i - 300)
    assert f.collect().to_arrow().num_rows == 4
    with pytest.raises(ur.ColumnNotFoundError):
        f.filter(col("nope") > 0)


def _per_step(build, steps: int) -> float:
    best = float("inf")
    for _ in range(3):
        t = time.perf_counter()
        build(steps)
        best = min(best, time.perf_counter() - t)
    return best


def _filters(n: int) -> None:
    f = _edges()
    for i in range(n):
        f = f.filter(col("s") > i - n)


def _graph_columns(n: int) -> None:
    e = _edges()
    f = e.nodes()
    for i in range(n):
        f = f.with_columns(**{f"d{i}": ur.degree(e)})


def _scalar_columns(n: int) -> None:
    f = _nodes()
    for i in range(n):
        f = f.with_columns(**{f"c{i}": col("v") + i})


@pytest.mark.parametrize("build", [_filters, _graph_columns, _scalar_columns])
def test_a_step_validates_in_time_independent_of_the_chain_before_it(build):
    # 4x the steps in well under 4x^2 the time: a step's validation reads its
    # input's, not the whole chain (or every column) again.
    small, large = _per_step(build, 200), _per_step(build, 800)
    assert large < 6 * small, (small, large)


def test_a_frame_joined_with_itself_validates_in_linear_time():
    # A self-join's validation reads its input's once (a collect still runs
    # each side, as it always has: 2**24 joins here).
    f = _nodes()
    t = time.perf_counter()
    for i in range(24):
        f = f.join(f, on="id", how="semi")
        if i == 5:
            assert f.schema().names == ["id", "label", "v"]
            assert f.collect().to_arrow().num_rows == 4
    assert time.perf_counter() - t < 1.0


def test_a_long_chain_drops_on_a_small_stack():
    # Dropping a chain unlinks it a node at a time, not a stack frame per step.
    code = textwrap.dedent(
        """
        import threading, pyarrow as pa, ursa as ur
        def run():
            f = ur.from_arrow(pa.table({"s": [1], "d": [2]}), src="s", dst="d")
            for _ in range(20_000):
                f = f.filter(ur.col("s") > 0)
            del f
            print("ok")
        threading.stack_size(256 * 1024)
        t = threading.Thread(target=run)
        t.start()
        t.join()
        """
    )
    out = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=300)
    assert out.returncode == 0 and out.stdout.strip() == "ok", out


def test_a_long_chain_explains_a_line_per_step():
    f = _edges()
    for _ in range(2000):
        f = f.filter(col("s") > 0)
    text = f.explain()
    plan = text.split("\n\n")[0].splitlines()
    assert len(plan) == 2002 and all(line.startswith("  ") and line[2] != " " for line in plan[1:])


# --- caches on the Rust state -----------------------------------------------------
def test_a_derivation_made_before_the_build_sees_the_build():
    edges = _edges()
    derived = [edges.select("s", "d", "w"), edges.filter(col("w") > 0), edges.sort("w")]
    before = _native()._topology_build_count()
    ur.degree(edges).collect()
    ur.degree(derived[0]).collect()
    ur.degree(derived[1]).collect()  # a view: the parent's index, masked
    assert _native()._topology_build_count() - before == 1
    assert derived[0]._index_build_cell.value is edges._index_build_cell.value
    # sort reshapes the rows: its graph is its rows', built once and cached.
    ur.degree(derived[2]).collect()
    ur.degree(derived[2]).collect()
    assert _native()._topology_build_count() - before == 2


def test_frames_build_and_collect_from_many_threads():
    edges = _edges()
    base = edges.nodes().with_columns(pr=ur.pagerank(edges))
    want = base.filter(col("pr") > 0).sort("pr").collect().to_arrow()
    results, errors = [], []
    barrier = threading.Barrier(8)

    def worker() -> None:
        barrier.wait()
        try:
            for _ in range(20):
                f = base.filter(col("pr") > 0).sort("pr")
                f.schema()
                results.append(f.collect().to_arrow())
                with pytest.raises(ur.ColumnNotFoundError):
                    base.filter(col("nope") > 0)
        except Exception as exc:  # pragma: no cover - reported below
            errors.append(exc)

    threads = [threading.Thread(target=worker) for _ in range(8)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert not errors
    assert len(results) == 160 and all(r.equals(want) for r in results)


# --- frame behaviours that stay ---------------------------------------------------
def test_frame_types_reprs_copies_and_hashing():
    e, n = _edges(), _nodes()
    assert isinstance(e.filter(col("w") > 0), ur.EdgeFrame)
    assert isinstance(e.nodes(), ur.NodeFrame)
    assert isinstance(n.join(n.select("id"), on="id"), ur.NodeFrame)
    assert isinstance(e.group_by("s", "d").agg(w=col("w").sum()), ur.EdgeFrame)
    assert isinstance(ur.hop(e, 1).from_([0]), ur.EdgeFrame)
    assert isinstance(ur.random_walk(e, start=[0], steps=1), ur.NodeFrame)
    assert repr(e) == "<EdgeFrame src='s' dst='d' lazy; 1 step(s)>"
    assert str(e.nodes().sort("id")) == "<NodeFrame id='id' lazy; 3 step(s)>"
    assert repr(ur.hop(e, 1).from_([0])) == "<EdgeFrame src='src' dst='dst' lazy; 1 step(s)>"
    # copy.copy shares the plan; deepcopy and pickle are not supported (as before).
    c = copy.copy(e)
    assert c is not e and c._node is e._node
    assert c.collect().to_arrow().equals(e.collect().to_arrow())
    with pytest.raises(TypeError):
        copy.deepcopy(e)
    with pytest.raises(TypeError):
        pickle.dumps(e)
    # Frames are hashable by identity, so they can be dict keys.
    assert {e: 1, n: 2}[e] == 1


# --- no Python module enumerates engine capabilities ------------------------------
# The words a list of the engine's steps, verbs, aggregations, joins, directions
# or modes would be made of. Rust is the one place that lists them (#193).
_CAPABILITIES = {
    "filter", "select", "with_columns", "rename", "sort", "head", "sample", "distinct",
    "join", "group_by_agg", "nodes", "reverse", "hop", "shortest_path", "random_walk",
    "describe", "from_arrow", "scan_edges", "scan_nodes", "pagerank", "degree",
    "connected_components", "triangle_count", "clustering_coefficient", "betweenness",
    "closeness", "label_propagation", "louvain", "neighbors_agg", "layout_fr",
    "layout_random", "layout_circle", "mean", "sum", "min", "max", "count", "n_unique",
    "inner", "left", "right", "outer", "full", "semi", "anti", "out", "in", "both",
    "error", "drop", "f32", "f64", "weak", "strong",
}  # fmt: skip


def test_no_python_module_enumerates_engine_capabilities():
    package = Path(ur.__file__).resolve().parent
    found = []
    for path in sorted(package.rglob("*.py")):
        tree = ast.parse(path.read_text(), filename=str(path))
        for node in ast.walk(tree):
            if not isinstance(node, (ast.List, ast.Tuple, ast.Set, ast.Dict)):
                continue
            items = node.keys if isinstance(node, ast.Dict) else node.elts
            words = [
                i.value for i in items if isinstance(i, ast.Constant) and isinstance(i.value, str)
            ]
            if len(words) >= 2 and len(words) == len(items) and set(words) <= _CAPABILITIES:
                found.append(f"{path.name}:{node.lineno}: {words}")
    assert not found, "Python lists engine capabilities (keep them in Rust):\n" + "\n".join(found)
