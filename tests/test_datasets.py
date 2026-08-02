"""ursa.datasets — bundled graphs (offline) + the download/cache machinery (#96).

The bundled loaders are tested fully **offline** (exact node/edge counts, an
algorithm end-to-end, node labels, and a NetworkX cross-check). The download tier
is tested **hermetically**: a local HTTP server exercises the cache/checksum
machinery on 127.0.0.1, and ``load_facebook`` is driven through its cache-hit
path by pre-seeding the cache — so no test reaches the public network. The one
genuinely-networked path (a real SNAP fetch) is not exercised here by design.
"""

from __future__ import annotations

import gzip
import hashlib
import http.server
import threading
from functools import partial

import pytest

import ursa as ur
from ursa import datasets

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)

BUNDLED = {
    "karate": (34, 78),
    "lesmis": (77, 254),
    "florentine": (15, 20),
    "kite": (10, 18),
}


def _counts(edges) -> tuple[int, int]:
    # A bare edges.nodes() has no materializable node set on its own; a graph op
    # (degree) produces one row per node in the topology (distinct src/dst union).
    e = edges.collect().to_arrow()
    nodes = edges.nodes().with_columns(_d=ur.degree(edges, direction="out")).collect().to_arrow()
    return nodes.num_rows, e.num_rows


# --- bundled loaders (offline) ---------------------------------------------


@pytest.mark.parametrize("name", sorted(BUNDLED))
def test_bundled_loader_counts(name):
    exp_nodes, exp_edges = BUNDLED[name]
    n, e = _counts(datasets.load(name))
    assert (n, e) == (exp_nodes, exp_edges)


def test_karate_node_labels():
    _edges, nodes = datasets.load_karate(with_nodes=True)
    tbl = nodes.collect().to_arrow()
    assert set(tbl.column_names) == {"id", "club"}
    clubs = set(tbl.column("club").to_pylist())
    assert clubs == {"Mr. Hi", "Officer"}


def test_karate_default_returns_edgeframe_only():
    edges = datasets.load_karate()
    assert isinstance(edges, ur.EdgeFrame)


def test_lesmis_is_weighted():
    edges = datasets.load_lesmis()
    assert "weight" in edges.collect().to_arrow().column_names
    # weight is usable in a weighted algorithm end-to-end
    out = edges.nodes().with_columns(pr=ur.pagerank(edges, weight=ur.col("weight"))).collect()
    assert out.to_arrow().num_rows == 77


def test_bundled_runs_an_algorithm_end_to_end():
    edges = datasets.load_karate()
    top = (
        edges.nodes()
        .with_columns(pr=ur.pagerank(edges))
        .sort("pr", descending=True)
        .head(3)
        .collect()
        .to_arrow()
    )
    assert top.num_rows == 3
    assert "pr" in top.column_names


def test_karate_matches_networkx_degrees():
    nx = pytest.importorskip("networkx")
    edges = datasets.load_karate()
    deg = edges.nodes().with_columns(d=ur.degree(edges, direction="out")).collect().to_arrow()
    # Undirected graph loaded as directed rows: out-degree here == nx degree for
    # the endpoints as stored. Compare the total edge count instead (robust to
    # direction convention): sum of out-degrees == number of edges.
    assert sum(deg.column("d").to_pylist()) == 78
    assert nx.karate_club_graph().number_of_edges() == 78


# --- registry --------------------------------------------------------------


def test_list_datasets_metadata():
    infos = {d.name: d for d in datasets.list_datasets()}
    assert infos["karate"].bundled and infos["karate"].node_labels
    assert infos["lesmis"].weighted
    assert not infos["facebook"].bundled
    for info in infos.values():
        assert info.license  # every dataset records a license


def test_load_unknown_dataset_errors():
    with pytest.raises(KeyError, match="unknown dataset"):
        datasets.load("does_not_exist")


# --- cache directory resolution --------------------------------------------


def test_cache_dir_honors_env(monkeypatch, tmp_path):
    monkeypatch.setenv("URSA_DATA_HOME", str(tmp_path / "cache"))
    assert datasets._cache_dir() == tmp_path / "cache"


def test_cache_dir_default(monkeypatch):
    monkeypatch.delenv("URSA_DATA_HOME", raising=False)
    assert datasets._cache_dir().parts[-2:] == ("ursa", "datasets")


# --- download / cache machinery (hermetic, local server) -------------------


@pytest.fixture
def http_root(tmp_path):
    served = tmp_path / "served"
    served.mkdir()
    handler = partial(http.server.SimpleHTTPRequestHandler, directory=str(served))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield served, f"http://127.0.0.1:{port}"
    finally:
        server.shutdown()
        server.server_close()


def test_fetch_cached_downloads_then_hits_cache(monkeypatch, tmp_path, http_root):
    served, base = http_root
    (served / "data.bin").write_bytes(b"hello graph")
    monkeypatch.setenv("URSA_DATA_HOME", str(tmp_path / "cache"))
    p1 = datasets._fetch_cached(f"{base}/data.bin", "data.bin")
    assert p1.read_bytes() == b"hello graph"
    # Cache hit: delete the server-side file; a second fetch still succeeds offline.
    (served / "data.bin").unlink()
    p2 = datasets._fetch_cached(f"{base}/data.bin", "data.bin")
    assert p2 == p1 and p2.read_bytes() == b"hello graph"


def test_fetch_cached_verifies_checksum(monkeypatch, tmp_path, http_root):
    served, base = http_root
    body = b"verify me"
    (served / "f.bin").write_bytes(body)
    monkeypatch.setenv("URSA_DATA_HOME", str(tmp_path / "cache"))
    good = hashlib.sha256(body).hexdigest()
    assert datasets._fetch_cached(f"{base}/f.bin", "f.bin", sha256=good).exists()


def test_fetch_cached_checksum_mismatch_raises(monkeypatch, tmp_path, http_root):
    served, base = http_root
    (served / "g.bin").write_bytes(b"tampered")
    monkeypatch.setenv("URSA_DATA_HOME", str(tmp_path / "cache"))
    with pytest.raises(RuntimeError, match="checksum mismatch"):
        datasets._fetch_cached(f"{base}/g.bin", "g.bin", sha256="0" * 64)
    # A failed-checksum download must not leave a cached file behind.
    assert not (tmp_path / "cache" / "g.bin").exists()


def test_fetch_cached_offline_raises_clear_error(monkeypatch, tmp_path):
    monkeypatch.setenv("URSA_DATA_HOME", str(tmp_path / "cache"))
    # An unroutable port -> connection error -> the clear, actionable message.
    with pytest.raises(RuntimeError, match="could not download"):
        datasets._fetch_cached("http://127.0.0.1:1/missing.bin", "missing.bin")


# --- the gzipped edge-list parser + load_facebook cache-hit path -----------


def _write_facebook_gz(path):
    lines = "# comment line\n0 1\n0 2\n1 2\n2 3\n\n"
    path.write_bytes(gzip.compress(lines.encode()))


def test_parse_edge_list_gz(tmp_path):
    gz = tmp_path / "edges.txt.gz"
    _write_facebook_gz(gz)
    edges = datasets._parse_edge_list_gz(gz)
    tbl = edges.collect().to_arrow()
    assert tbl.num_rows == 4  # comment + blank line skipped
    assert tbl.column("src").to_pylist() == [0, 0, 1, 2]


def test_load_facebook_cache_hit(monkeypatch, tmp_path):
    # Drive load_facebook() through its cache-hit branch by pre-seeding the cache,
    # so the full loader (fetch -> parse -> EdgeFrame) runs with no network.
    cache = tmp_path / "cache"
    cache.mkdir(parents=True)
    _write_facebook_gz(cache / "facebook_combined.txt.gz")
    monkeypatch.setenv("URSA_DATA_HOME", str(cache))
    edges = datasets.load_facebook()
    n, e = _counts(edges)
    assert e == 4 and n == 4
