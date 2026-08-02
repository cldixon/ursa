"""Public graph datasets for examples and benchmarks.

Two tiers:

- **Bundled** — tiny, canonical graphs shipped inside the wheel as CSVs, so they
  load fully offline with no download: Zachary's karate club (with faction
  labels), the Les Misérables co-appearance network (weighted), the Florentine
  families marriage network, and the Krackhardt kite (the textbook
  centrality-divergence example). All are redistributed from NetworkX (BSD).

- **Downloaded** — larger, realistic benchmark graphs fetched on first use and
  cached under ``$URSA_DATA_HOME`` (default ``~/.cache/ursa/datasets``): the SNAP
  ego-Facebook network. A cache hit is offline; a cache miss with no network
  raises a clear error rather than silently substituting anything.

Every loader returns an :class:`~ursa.EdgeFrame` — the graph — and labelled
datasets expose their node attributes as a separate :class:`~ursa.NodeFrame` via
``with_nodes=True`` (returning ``(edges, nodes)``), matching Ursa's model where a
NodeFrame is an *optional* attribute table joined to the graph by id.

    import ursa as ur
    edges, nodes = ur.datasets.load_karate(with_nodes=True)
    ur.pagerank(edges).collect().to_polars()
"""

from __future__ import annotations

import gzip
import hashlib
import os
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Literal, NamedTuple, overload

from .._frames import EdgeFrame, NodeFrame

_DATA = Path(__file__).parent / "data"


class DatasetInfo(NamedTuple):
    """One row of the dataset registry — metadata for :func:`list_datasets`."""

    name: str
    nodes: int
    edges: int
    bundled: bool  # True: shipped in the wheel (offline); False: downloaded + cached
    weighted: bool
    node_labels: bool  # True if a NodeFrame of attributes is available
    description: str
    source: str
    license: str


_REGISTRY: dict[str, DatasetInfo] = {
    "karate": DatasetInfo(
        "karate",
        34,
        78,
        True,
        False,
        True,
        "Zachary's karate club — the canonical small social network, with each "
        "member's faction ('Mr. Hi' vs 'Officer') as a node label.",
        "W. W. Zachary (1977), via networkx.karate_club_graph",
        "BSD (networkx)",
    ),
    "lesmis": DatasetInfo(
        "lesmis",
        77,
        254,
        True,
        True,
        False,
        "Les Misérables character co-appearance network; edge weight is the number "
        "of chapters two characters share.",
        "D. E. Knuth (1993), via networkx.les_miserables_graph",
        "BSD (networkx)",
    ),
    "florentine": DatasetInfo(
        "florentine",
        15,
        20,
        True,
        False,
        False,
        "Marriage ties among Renaissance Florentine families.",
        "Padgett & Ansell (1993), via networkx.florentine_families_graph",
        "BSD (networkx)",
    ),
    "kite": DatasetInfo(
        "kite",
        10,
        18,
        True,
        False,
        False,
        "Krackhardt's kite — the textbook example where degree, betweenness, and "
        "closeness centrality each rank a different node highest.",
        "D. Krackhardt (1990), via networkx.krackhardt_kite_graph",
        "BSD (networkx)",
    ),
    "facebook": DatasetInfo(
        "facebook",
        4039,
        88234,
        False,
        False,
        False,
        "SNAP ego-Facebook: friendship circles from survey participants "
        "(undirected). Downloaded and cached on first use.",
        "https://snap.stanford.edu/data/ego-Facebook.html",
        "SNAP terms (academic use)",
    ),
}

# The SNAP ego-Facebook combined edge list (a gzipped, space-separated edge list).
# The checksum is intentionally unpinned (None): this build environment cannot
# reach snap.stanford.edu to compute it, and shipping an unverified hash would be
# worse than none. Pinning it is a follow-up once fetched from an open network.
_FACEBOOK_URL = "https://snap.stanford.edu/data/facebook_combined.txt.gz"
_FACEBOOK_SHA256: str | None = None


# --- bundled loaders -------------------------------------------------------
def _read_bundled_edges(filename: str, *, src: str = "src", dst: str = "dst") -> EdgeFrame:
    import pyarrow.csv as pacsv

    table = pacsv.read_csv(_DATA / filename)
    return EdgeFrame(table, src=src, dst=dst)


def _read_bundled_nodes(filename: str, *, id: str = "id") -> NodeFrame:
    import pyarrow.csv as pacsv

    return NodeFrame(pacsv.read_csv(_DATA / filename), id=id)


@overload
def load_karate(*, with_nodes: Literal[False] = ...) -> EdgeFrame: ...
@overload
def load_karate(*, with_nodes: Literal[True]) -> tuple[EdgeFrame, NodeFrame]: ...
def load_karate(*, with_nodes: bool = False) -> EdgeFrame | tuple[EdgeFrame, NodeFrame]:
    """Zachary's karate club (34 nodes, 78 edges). With ``with_nodes=True`` also
    returns a NodeFrame of each member's faction (``club``)."""
    edges = _read_bundled_edges("karate_edges.csv")
    if with_nodes:
        return edges, _read_bundled_nodes("karate_nodes.csv")
    return edges


def load_lesmis() -> EdgeFrame:
    """Les Misérables co-appearance network (77 nodes, 254 weighted edges). The
    ``weight`` column is usable in weighted algorithms, e.g.
    ``ur.pagerank(edges, weight=ur.col('weight'))``."""
    return _read_bundled_edges("lesmis_edges.csv")


def load_florentine() -> EdgeFrame:
    """Florentine families marriage network (15 nodes, 20 edges)."""
    return _read_bundled_edges("florentine_edges.csv")


def load_kite() -> EdgeFrame:
    """Krackhardt's kite graph (10 nodes, 18 edges)."""
    return _read_bundled_edges("kite_edges.csv")


# --- download + cache ------------------------------------------------------
def _cache_dir() -> Path:
    """Where downloaded datasets are cached: ``$URSA_DATA_HOME`` if set, else
    ``~/.cache/ursa/datasets`` (XDG-style, computed without a platformdirs dep)."""
    base = os.environ.get("URSA_DATA_HOME")
    root = Path(base) if base else Path.home() / ".cache" / "ursa" / "datasets"
    return root


def _fetch_cached(url: str, filename: str, *, sha256: str | None = None) -> Path:
    """Return a local path to ``filename``, downloading ``url`` into the cache on a
    miss. A cache hit never touches the network. On a miss with no network, raises
    a clear, actionable error (never a silent fallback). The write is atomic (temp
    file + rename) so an interrupted download can't leave a corrupt cache entry;
    ``sha256`` (when given) is verified before the file is committed."""
    dest = _cache_dir() / filename
    if dest.exists():
        return dest
    dest.parent.mkdir(parents=True, exist_ok=True)
    try:
        with urllib.request.urlopen(url, timeout=60) as resp:
            data = resp.read()
    except (urllib.error.URLError, OSError) as exc:
        raise RuntimeError(
            f"could not download {url!r} to the dataset cache ({exc}). This dataset "
            f"is not bundled; it needs one download (cached afterwards at {dest}). "
            "Set URSA_DATA_HOME to a writable location, or pre-fetch the file, then retry."
        ) from exc
    if sha256 is not None:
        got = hashlib.sha256(data).hexdigest()
        if got != sha256:
            raise RuntimeError(
                f"checksum mismatch for {url!r}: expected {sha256}, got {got}. The "
                "download may be corrupt or the source changed; not caching it."
            )
    tmp = dest.with_name(dest.name + ".tmp")
    tmp.write_bytes(data)
    tmp.replace(dest)
    return dest


def _parse_edge_list_gz(path: Path, *, src: str = "src", dst: str = "dst") -> EdgeFrame:
    """Parse a gzipped, whitespace-separated ``u v`` edge list (the SNAP format)
    into an EdgeFrame with int node ids. Blank lines and ``#`` comments are
    skipped."""
    import pyarrow as pa

    srcs: list[int] = []
    dsts: list[int] = []
    with gzip.open(path, "rt") as f:
        for line in f:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            u, v = line.split()[:2]
            srcs.append(int(u))
            dsts.append(int(v))
    table = pa.table({src: pa.array(srcs, pa.int64()), dst: pa.array(dsts, pa.int64())})
    return EdgeFrame(table, src=src, dst=dst)


def load_facebook() -> EdgeFrame:
    """SNAP ego-Facebook friendship network (4039 nodes, 88234 undirected edges).

    Downloaded from SNAP on first use and cached under ``$URSA_DATA_HOME`` (default
    ``~/.cache/ursa/datasets``); subsequent loads are offline. Requires network on
    the first call — a cache miss with no network raises a clear error.
    """
    path = _fetch_cached(_FACEBOOK_URL, "facebook_combined.txt.gz", sha256=_FACEBOOK_SHA256)
    return _parse_edge_list_gz(path)


# --- registry access -------------------------------------------------------
_LOADERS: dict[str, Any] = {
    "karate": load_karate,
    "lesmis": load_lesmis,
    "florentine": load_florentine,
    "kite": load_kite,
    "facebook": load_facebook,
}


def list_datasets() -> list[DatasetInfo]:
    """Every available dataset with its metadata (node/edge counts, whether it is
    bundled or downloaded, weightedness, node labels, source, and license)."""
    return list(_REGISTRY.values())


def load(name: str, **kwargs: Any) -> Any:
    """Load a dataset by name (see :func:`list_datasets` for the names). Keyword
    arguments are forwarded to the specific loader (e.g. ``with_nodes=True`` for
    ``karate``)."""
    try:
        loader = _LOADERS[name]
    except KeyError:
        raise KeyError(
            f"unknown dataset {name!r}; available: {', '.join(sorted(_LOADERS))}."
        ) from None
    return loader(**kwargs)


__all__ = [
    "DatasetInfo",
    "list_datasets",
    "load",
    "load_facebook",
    "load_florentine",
    "load_karate",
    "load_kite",
    "load_lesmis",
]
