---
title: Reading data
description: scan_edges and scan_nodes over Parquet and CSV, local or on S3, GCS and Azure, with the column projection pushed into the file.
subtitle: Lazy by default. Nothing is read until collect(), and then only the columns the plan needs.
---

## From a file

The simplest graph dataset is a two-column edge list.

```python
import ursa as ur

edges = ur.scan_edges("web-google.csv", src="FromNodeId", dst="ToNodeId")
```

`scan_edges` is lazy — nothing is read yet. `src=` and `dst=` are **role mappings**, not renames:
the frame remembers which columns play the source and destination roles, and the original column
names are preserved for round-tripping. Introspect them with `edges.src_col` and `edges.dst_col`.

With no node file, the node set is derived from the edges:

```python
nodes = edges.nodes()   # lazy NodeFrame: the distinct union of src and dst
```

A separate node table is a second scan, keyed by id:

```python
nodes = ur.scan_nodes("towers.parquet", id="tower_id")
```

Formats are **Parquet and CSV**. Glob patterns work; a *list* of paths is accepted by the
signature but raises at collect — one path or glob per scan for now.

```python
ur.scan_edges("data/part-*.parquet", src="s", dst="d")
```

A single hosted file also reads over plain **HTTP(S)**:

```python
edges = ur.scan_edges("https://example.com/data/edges.parquet", src="s", dst="d")
```

No globbing over HTTP, and query strings are dropped before fetching — so a presigned or
token-bearing URL will not authenticate; use the object-store scheme and `storage_options` for
that.

## From memory

The frame types are public, data-first constructors — a list of row dicts, a dict of columns, a
polars or pandas DataFrame, or anything Arrow-backed. No `pyarrow` import required:

```python
edges = ur.EdgeFrame({"s": [1, 2, 3, 0], "d": [0, 0, 0, 1]}, src="s", dst="d")
nodes = ur.NodeFrame([{"id": 0, "team": "red"}, {"id": 1, "team": "blue"}], id="id")
```

Typed aliases exist for each source, zero-copy where the source is Arrow-backed, and the wider
ecosystem has direct entry points — networkx, numpy and scipy are imported lazily, never
depended on:

```python
edges = ur.from_polars(df, src="a", dst="b")     # or id= for a NodeFrame
edges = ur.from_pandas(df, src="a", dst="b")
edges = ur.from_arrow(tbl, src="a", dst="b")

edges = ur.from_edgelist([(0, 1), (1, 2), (2, 0)])
edges = ur.from_edgelist([(0, 1, 0.5), (1, 2, 2.0)], weighted="w")
edges = ur.from_networkx(G, weight="weight")
nodes = ur.nodes_from_networkx(G)                # node attributes as a NodeFrame
edges = ur.from_numpy(adjacency)                 # dense adjacency or an edge array
edges = ur.from_scipy_sparse(matrix)
```

However the frame was built, it behaves identically from there on. Node ids may be int64 (the
fast path) or strings such as UUIDs — auto-detected, with results keyed by the original ids.

## Bundled datasets

`ur.datasets` ships small canonical graphs, so examples and tests need no files at all:

```python
edges = ur.datasets.load_karate()                  # 34 nodes, 78 edges, offline
edges, clubs = ur.datasets.load_karate(with_nodes=True)
edges = ur.datasets.load_lesmis()                  # weighted
edges = ur.datasets.load_facebook()                # SNAP ego-Facebook; downloaded once, cached

ur.datasets.list_datasets()
```

The bundled sets (`karate`, `lesmis`, `florentine`, `kite`) load offline from the wheel;
`facebook` downloads on first use and caches under `$URSA_DATA_HOME`
(default `~/.cache/ursa/datasets`).

## Eager conveniences

`read_edges` and `read_nodes` are `scan` + `collect` in one call, for when you know you want the
data now.

```python
tbl = ur.read_edges("links.parquet", src="s", dst="d")   # a MaterializedFrame
```

## Object storage

Object storage is first-class. Change the scheme and pass credentials or configuration through
`storage_options`; the values layer over the backend's own default credential chain (environment
variables, instance profile), so in a properly configured environment you often need nothing at
all.

```python
edges = ur.scan_edges(
    "s3://telco-lake/graph/links/*.parquet",
    src="tower_a", dst="tower_b",
    storage_options={"region": "us-east-1"},
)
nodes = ur.scan_nodes("s3://telco-lake/graph/towers/*.parquet", id="tower_id")

ur.pagerank(edges).collect().to_polars()
```

| Scheme | Backend |
|---|---|
| `s3://` | AWS S3 and S3-compatible endpoints |
| `gs://` | Google Cloud Storage |
| `az://` | Azure Blob Storage |
| `file://` | The local filesystem, explicitly |

An S3-compatible endpoint (MinIO, or a local emulator) is reached with the usual options:

```python
storage_options={
    "endpoint_url": "http://localhost:9000",
    "access_key_id": "...",
    "secret_access_key": "...",
    "allow_http": "true",
}
```

Paths must name a file or a glob today — `s3://bucket/data/*.parquet` works, a bare prefix
`s3://bucket/data/` does not.

## Projection pushdown

An edge scan reads only the columns the plan proves it needs. That is not a micro-optimization on
a wide table in object storage: it is often the difference between reading two columns and reading
forty.

```python
# reads exactly `s` and `d` from the Parquet file — nothing else
ur.pagerank(ur.scan_edges("edges.parquet", src="s", dst="d")).collect()
```

The same holds for a node scan narrowed by `select(...)`: the projection propagates back into the
file, so a `select` at the end of a pipeline changes what is read at the start of it.

Predicate pushdown is not implemented yet; filters run in the engine after the read.

## Not yet wired

`scan_*` reserves a `store=` parameter for a pre-configured
[obstore](https://github.com/developmentseed/obstore) store object in place of `storage_options`.
It is **not supported yet** and raises rather than being silently ignored — the two libraries are
separate Rust extension modules, each with its own statically linked copy of `object_store`, so
sharing a store across that boundary needs a real interop mechanism, not a pointer.

Format options (`**format_opts`, for example a CSV `delimiter=`) are accepted by the signature and
also raise for now. The rule is consistent throughout Ursa: a parameter that cannot be honoured
raises, rather than being accepted and quietly dropped.

## Null endpoints

A null `src` or `dst` in the edge input **raises** by default: a silently dropped row is a
silently wrong answer. When dropping is what you want, say so:

```python
edges = ur.scan_edges("links.parquet", src="s", dst="d", on_null="drop")
```

`on_null="drop"` filters out edge rows with a null endpoint and reports the count as a Python
warning — `on_null='drop': dropped N edge row(s)` — so a drop can never masquerade as "all rows
ingested". It is accepted everywhere edges come in: `scan_edges`, `read_edges`, the `EdgeFrame`
constructor, `from_arrow` and `from_edgelist`. Node scans have no `on_null` — the policy is about
edge endpoints.
