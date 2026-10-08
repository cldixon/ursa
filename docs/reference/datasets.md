# Datasets

`ur.datasets` has small example graphs. Four are bundled in the wheel and load offline. One is downloaded on first use.

## Loaders

Every loader returns an `EdgeFrame` with the columns `src`, `dst`, and (if weighted) `weight`.

| Function | Nodes | Edges | Weighted | Node labels | Description |
|---|---|---|---|---|---|
| `ur.datasets.load_karate(*, with_nodes=False)` | 34 | 78 | no | `club` | Zachary's karate club. |
| `ur.datasets.load_lesmis()` | 77 | 254 | yes | | Les Misérables co-appearance. The weight is the number of shared chapters. |
| `ur.datasets.load_florentine()` | 15 | 20 | no | | Florentine families marriage ties. |
| `ur.datasets.load_kite()` | 10 | 18 | no | | Krackhardt's kite. |
| `ur.datasets.load_facebook()` | 4039 | 88234 | no | | SNAP ego-Facebook. Downloaded on first use. |

`load_karate(with_nodes=True)` returns a tuple `(edges, nodes)`. `nodes` is a `NodeFrame` with the columns `id` and `club`.

Each bundled graph is undirected in its source. The loader stores each edge once, as one row. Pass `direction="both"` to `degree`, `hop`, and `shortest_path` for the undirected result.

## `ur.datasets.load`

```py
ur.datasets.load(name: str, **kwargs)
```

Loads a dataset by name. `kwargs` go to the loader, for example `load("karate", with_nodes=True)`.

## `ur.datasets.list_datasets`

```py
ur.datasets.list_datasets() -> list[DatasetInfo]
```

The registry. Each `DatasetInfo` is a named tuple with the fields `name`, `nodes`, `edges`, `bundled`, `weighted`, `node_labels`, `description`, `source`, and `license`.

## The download cache

`load_facebook()` downloads one file from SNAP on the first call and verifies its checksum. The file is cached in `$URSA_DATA_HOME`, or in `~/.cache/ursa/datasets` if the variable is not set. Later calls read the cache and need no network. If the download fails, the loader raises an error that names the URL and the cache path.

## Licenses

The bundled graphs are redistributed from NetworkX (BSD license) as plain CSV files. The downloaded graph is subject to the SNAP terms. The file `LICENSES.md` in the installed package lists the original sources.
