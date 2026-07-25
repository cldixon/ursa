"""Real-graph loaders → Parquet edge lists.

Real graphs carry the degree skew and structure synthetic ones don't, plus the
credibility of standard benchmark sets. Here that means SNAP edge-list dumps
(``.txt.gz``): downloaded once, checksummed if a digest is known, parsed, and
cached as Parquet. Original node ids are preserved (arbitrary int64 is fine for
all three libraries), so nothing is silently renumbered.

Downloads honour the environment's HTTPS proxy (``urllib`` reads ``HTTPS_PROXY``).
Real datasets are large and are never run in CI — only synthetic smoke is.
"""

from __future__ import annotations

import gzip
import hashlib
import urllib.request
from pathlib import Path

import pyarrow as pa
import pyarrow.parquet as pq


def _download(url: str, dest: Path, sha256: str | None) -> Path:
    dest.parent.mkdir(parents=True, exist_ok=True)
    if not dest.exists():
        tmp = dest.with_suffix(dest.suffix + ".part")
        with urllib.request.urlopen(url, timeout=120) as resp, open(tmp, "wb") as fh:
            while chunk := resp.read(1 << 20):
                fh.write(chunk)
        tmp.replace(dest)
    if sha256:
        digest = hashlib.sha256(dest.read_bytes()).hexdigest()
        if digest != sha256:
            dest.unlink(missing_ok=True)
            raise ValueError(f"checksum mismatch for {url}: got {digest}, want {sha256}")
    return dest


def snap_edge_list(
    url: str,
    parquet_path: str | Path,
    *,
    cache_dir: str | Path,
    sha256: str | None = None,
) -> Path:
    """Download a SNAP ``.txt.gz`` edge list and cache it as Parquet.

    The SNAP text format is whitespace-separated ``src dst`` with ``#`` comment
    lines; splitting on any whitespace handles both the tab- and space-delimited
    variants across their datasets.
    """
    parquet_path = Path(parquet_path)
    if parquet_path.exists():
        return parquet_path

    raw = _download(url, Path(cache_dir) / "downloads" / Path(url).name, sha256)

    src: list[int] = []
    dst: list[int] = []
    with gzip.open(raw, "rt") as fh:
        for line in fh:
            if not line or line[0] == "#":
                continue
            parts = line.split()
            if len(parts) < 2:
                continue
            src.append(int(parts[0]))
            dst.append(int(parts[1]))

    parquet_path.parent.mkdir(parents=True, exist_ok=True)
    table = pa.table({"src": pa.array(src, pa.int64()), "dst": pa.array(dst, pa.int64())})
    pq.write_table(table, parquet_path)
    return parquet_path
