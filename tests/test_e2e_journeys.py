"""End-to-end user journeys (issue #99).

These are not unit tests of one verb — each is a realistic path a user takes
through the *whole* library: get a graph in (a bundled dataset, a networkx graph,
a scipy matrix, a hosted URL), build it, run algorithms, shape the result with
the relational dialect, and get data back out (and re-read it). They exercise the
surface widened across the data-&-realism track (datasets, interop constructors,
URL scans) composed with the engine (algorithms, group_by, filters, weighted
ops). Everything is hermetic: bundled data + a local HTTP server, no public
network. Assertions target stable structural properties, not brittle floats.
"""

from __future__ import annotations

import http.server
import threading
from functools import partial

import pyarrow.parquet as pq
import pytest

import ursa as ur
from ursa import datasets

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


# --- Journey 1: bundled dataset -> analyze -> shape -> egress round-trip ----


def test_journey_dataset_to_ranking_and_parquet_roundtrip(tmp_path):
    # A user loads a real graph, ranks nodes by two centralities, keeps the top
    # slice, writes it to Parquet, and reads it back — the "same from here on"
    # guarantee across an egress round-trip.
    edges = datasets.load_lesmis()
    result = (
        edges.nodes()
        .with_columns(
            pr=ur.pagerank(edges, weight=ur.col("weight")),
            deg=ur.degree(edges, direction="out"),
        )
        .sort("pr", descending=True)
        .head(10)
        .collect()
    )
    tbl = result.to_arrow()
    assert tbl.num_rows == 10
    assert {"id", "pr", "deg"} <= set(tbl.column_names)
    # pagerank is a probability distribution over all nodes: the full vector sums
    # to ~1, so every kept value is in (0, 1].
    prs = tbl.column("pr").to_pylist()
    assert all(0.0 < p <= 1.0 for p in prs)
    assert prs == sorted(prs, reverse=True)  # sort held

    out = tmp_path / "top.parquet"
    result.sink_parquet(str(out))
    back = pq.read_table(out)
    assert back.num_rows == 10
    assert back.column("id").to_pylist() == tbl.column("id").to_pylist()


# --- Journey 2: networkx interop -> community detection -> nx cross-check ---


def test_journey_networkx_to_communities():
    nx = pytest.importorskip("networkx")
    g = nx.karate_club_graph()
    edges = ur.from_networkx(g)
    out = (
        edges.nodes()
        .with_columns(comm=ur.louvain(edges), cc=ur.clustering_coefficient(edges))
        .collect()
        .to_arrow()
    )
    assert out.num_rows == g.number_of_nodes()  # every node labelled
    # Louvain finds more than one community in the karate club (it famously splits
    # into ~2-4), but never one-per-node or a single blob.
    n_comms = len(set(out.column("comm").to_pylist()))
    assert 1 < n_comms < g.number_of_nodes()
    # clustering coefficient is a ratio in [0, 1].
    assert all(0.0 <= c <= 1.0 for c in out.column("cc").to_pylist())


# --- Journey 3: scipy adjacency -> structure -> polars egress --------------


def test_journey_scipy_adjacency_to_polars():
    sp = pytest.importorskip("scipy.sparse")
    np = pytest.importorskip("numpy")
    # A directed 4-cycle 0->1->2->3->0 as a sparse adjacency matrix.
    rows = np.array([0, 1, 2, 3])
    cols = np.array([1, 2, 3, 0])
    adj = sp.coo_matrix((np.ones(4), (rows, cols)), shape=(4, 4))
    edges = ur.from_scipy_sparse(adj)
    df = (
        edges.nodes()
        .with_columns(
            indeg=ur.degree(edges, direction="in"),
            outdeg=ur.degree(edges, direction="out"),
        )
        .sort("id")
        .collect()
        .to_polars()
    )
    assert df.height == 4
    # Every node in a simple cycle has in-degree 1 and out-degree 1.
    assert df["indeg"].to_list() == [1, 1, 1, 1]
    assert df["outdeg"].to_list() == [1, 1, 1, 1]


# --- Journey 4: export -> host over HTTP -> scan back -> weighted pagerank --


class _RangeHandler(http.server.SimpleHTTPRequestHandler):
    """HEAD + Range GET, the minimum object_store needs to size and read a file."""

    def do_HEAD(self):
        import os

        path = self.translate_path(self.path)
        try:
            size = os.path.getsize(path)
        except OSError:
            self.send_error(404)
            return
        self.send_response(200)
        self.send_header("Content-Length", str(size))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()

    def do_GET(self):
        path = self.translate_path(self.path)
        try:
            with open(path, "rb") as f:
                body = f.read()
        except OSError:
            self.send_error(404)
            return
        rng = self.headers.get("Range")
        if rng and rng.startswith("bytes="):
            start_s, _, end_s = rng[len("bytes=") :].partition("-")
            start = int(start_s) if start_s else 0
            end = int(end_s) if end_s else len(body) - 1
            end = min(end, len(body) - 1)
            chunk = body[start : end + 1]
            self.send_response(206)
            self.send_header("Content-Range", f"bytes {start}-{end}/{len(body)}")
            self.send_header("Content-Length", str(len(chunk)))
            self.end_headers()
            self.wfile.write(chunk)
        else:
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    def log_message(self, format, *args):
        pass


@pytest.fixture
def http_root(tmp_path):
    served = tmp_path / "served"
    served.mkdir()
    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", 0), partial(_RangeHandler, directory=str(served))
    )
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield served, f"http://127.0.0.1:{port}"
    finally:
        server.shutdown()
        server.server_close()


def test_journey_export_then_scan_over_http(http_root, tmp_path):
    # A user exports an in-memory weighted graph to Parquet, hosts it, then a
    # second "consumer" reads it straight from the URL and runs a weighted op —
    # the ingress (URL scan) and egress (sink) halves meeting end to end.
    served, base = http_root
    edges = ur.from_edgelist([(0, 1, 5.0), (0, 2, 1.0), (1, 2, 1.0), (2, 3, 1.0)], weighted=True)
    edges.collect().sink_parquet(str(served / "edges.parquet"))

    scanned = ur.scan_edges(f"{base}/edges.parquet", src="src", dst="dst")
    ranked = (
        scanned.nodes()
        .with_columns(pr=ur.pagerank(scanned, weight=ur.col("weight")))
        .sort("pr", descending=True)
        .collect()
        .to_arrow()
    )
    assert ranked.num_rows == 4
    assert all(0.0 < p <= 1.0 for p in ranked.column("pr").to_pylist())


# --- Journey 5: labelled dataset -> centrality -> group_by the label -------


def test_journey_labels_centrality_grouped():
    # Load a labelled graph, compute a per-node centrality, then aggregate it by
    # the node label — a common analytics question ("how central is each group?").
    edges, nodes = datasets.load_karate(with_nodes=True)
    grouped = (
        nodes.with_columns(pr=ur.pagerank(edges), deg=ur.degree(edges, direction="out"))
        .group_by("club")
        .agg(
            avg_pr=ur.col("pr").mean(),
            total_deg=ur.col("deg").sum(),
            members=ur.col("pr").count(),
        )
        .sort("club")
        .collect()
        .to_arrow()
    )
    rows = {r["club"]: r for r in grouped.to_pylist()}
    assert set(rows) == {"Mr. Hi", "Officer"}
    # Every member is counted exactly once across the two factions.
    assert rows["Mr. Hi"]["members"] + rows["Officer"]["members"] == 34
    # The bundled karate CSV stores each undirected edge once as a directed row,
    # so out-degree counts each of the 78 edges once — the faction totals sum to |E|.
    assert rows["Mr. Hi"]["total_deg"] + rows["Officer"]["total_deg"] == 78
    assert all(0.0 < r["avg_pr"] < 1.0 for r in rows.values())


# --- Journey 6: traversal reachability on a real graph ---------------------


def test_journey_traversal_and_shortest_path():
    # Reachability + a shortest path on a real graph, ending in to_dicts egress.
    edges = datasets.load_karate()
    # Two-hop neighbourhood of node 0 (the instructor) — non-empty and bounded.
    reach = ur.hop(edges, n=2).from_([0]).collect().to_arrow()
    assert reach.num_rows > 0
    assert set(reach.column_names) >= {"src", "dst"}
    # A shortest path between two nodes returns the path edges as (src, dst) rows.
    path = ur.shortest_path(edges, 0, 33).collect().to_dicts()
    assert len(path) >= 1
    assert all("src" in row and "dst" in row for row in path)
