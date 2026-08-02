"""Reading edge/node files from an ``http(s)://`` URL (issue #98).

`scan_edges`/`scan_nodes` accept a plain HTTP(S) URL for a single hosted
Parquet/CSV file, routed through object_store's `http` backend. These tests are
fully **hermetic**: a local range-capable HTTP server serves a temp file on
127.0.0.1 — no public network is touched — so they are safe in CI. The server
supports HEAD (object_store fetches the size) and `Range` GETs (Parquet reads
the footer + row groups via ranged requests).
"""

from __future__ import annotations

import http.server
import os
import threading
from functools import partial

import pyarrow as pa
import pyarrow.parquet as pq
import pytest

import ursa as ur

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `maturin develop`)"
)


class _RangeHandler(http.server.SimpleHTTPRequestHandler):
    """Serve files from a directory with HEAD + `Range` GET support — the minimum
    object_store's HTTP store needs to size an object and read it by byte range."""

    def do_HEAD(self):
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
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            self.wfile.write(chunk)
        else:
            self.send_response(200)
            self.send_header("Content-Length", str(len(body)))
            self.send_header("Accept-Ranges", "bytes")
            self.end_headers()
            self.wfile.write(body)

    def log_message(self, format, *args):  # silence the per-request stderr spam
        pass


@pytest.fixture
def http_root(tmp_path):
    """A local HTTP server rooted at a temp dir. Yields ``(tmp_path, base_url)``."""
    handler = partial(_RangeHandler, directory=str(tmp_path))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    port = server.server_address[1]
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield tmp_path, f"http://127.0.0.1:{port}"
    finally:
        server.shutdown()
        server.server_close()


# The reference graph: 0->1, 0->2, 1->2, 2->3.  Out-degrees: 0:2, 1:1, 2:1, 3:0.
def _write_csv(dirpath, name="edges.csv"):
    (dirpath / name).write_text("s,d\n0,1\n0,2\n1,2\n2,3\n")
    return name


def _write_parquet(dirpath, name="edges.parquet"):
    tbl = pa.table({"s": [0, 0, 1, 2], "d": [1, 2, 2, 3]})
    pq.write_table(tbl, dirpath / name)
    return name


def _out_degrees(edges):
    frame = edges.nodes().with_columns(deg=ur.degree(edges, direction="out"))
    d = frame.collect().to_arrow().to_pydict()
    return dict(zip(d["id"], d["deg"], strict=True))


EXPECTED = {0: 2, 1: 1, 2: 1, 3: 0}


def test_scan_edges_over_http_csv(http_root):
    tmp_path, base = http_root
    name = _write_csv(tmp_path)
    edges = ur.scan_edges(f"{base}/{name}", src="s", dst="d")
    assert _out_degrees(edges) == EXPECTED


def test_scan_edges_over_http_parquet(http_root):
    # Parquet exercises ranged GETs (footer + row groups), not just a whole-file read.
    tmp_path, base = http_root
    name = _write_parquet(tmp_path)
    edges = ur.scan_edges(f"{base}/{name}", src="s", dst="d")
    assert _out_degrees(edges) == EXPECTED


def test_read_edges_over_http_then_pagerank(http_root):
    tmp_path, base = http_root
    name = _write_parquet(tmp_path)
    edges = ur.scan_edges(f"{base}/{name}", src="s", dst="d")
    ranked = edges.nodes().with_columns(pr=ur.pagerank(edges)).collect().to_arrow()
    assert "pr" in ranked.column_names
    assert ranked.num_rows == 4


def test_http_url_with_query_string_resolves_format(http_root):
    # A presigned-style URL (trailing ?query) must still resolve its .csv format —
    # the extension check strips the query string.
    tmp_path, base = http_root
    name = _write_csv(tmp_path)
    edges = ur.scan_edges(f"{base}/{name}?token=abc123", src="s", dst="d")
    assert _out_degrees(edges) == EXPECTED


def test_scan_nodes_over_http(http_root):
    tmp_path, base = http_root
    (tmp_path / "nodes.csv").write_text("id,region\n0,us\n1,us\n2,eu\n3,eu\n")
    out = ur.scan_nodes(f"{base}/nodes.csv", id="id").collect().to_arrow()
    assert out.num_rows == 4
    assert set(out.column_names) == {"id", "region"}


def test_http_404_errors_clearly(http_root):
    _, base = http_root
    edges = ur.scan_edges(f"{base}/does_not_exist.csv", src="s", dst="d")
    with pytest.raises(Exception):  # noqa: B017 - object_store surfaces a fetch error
        edges.collect()
