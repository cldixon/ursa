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

Formats in v0.1 are **Parquet and CSV**. Glob patterns work; a list of paths works.

```python
ur.scan_edges("data/part-*.parquet", src="s", dst="d")
ur.scan_edges(["a.parquet", "b.parquet"], src="s", dst="d")
```

## From memory

Ingress from an existing frame is symmetric and zero-copy — it is all Arrow underneath.

```python
edges = ur.from_polars(df, src="a", dst="b")
edges = ur.from_arrow(tbl, src="a", dst="b")

nodes = ur.from_polars(df, id="node_id")
nodes = ur.from_arrow(tbl, id="node_id")
```

The same functions build a `NodeFrame` when you pass `id=` instead of `src=`/`dst=`.

Node ids may be int64 (the fast path) or strings such as UUIDs. The type is auto-detected from the
column, and results come back keyed by the original ids.

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

A null `src` or `dst` in the edge input **raises** at index build. That is the safe default: a
silently dropped row is a silently wrong answer. An opt-in `on_null="drop"` is planned, and will
report how many rows it dropped when it lands.
