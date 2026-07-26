---
title: Install
description: Install ursa-graph from PyPI. Wheels bundle the Rust core, so no Rust toolchain is required.
subtitle: Distribution ursa-graph · import name ursa · Python ≥ 3.10
---

```bash
pip install ursa-graph
# or
uv add ursa-graph
```

The distribution is named `ursa-graph`; the import name is `ursa`.

```python
import ursa as ur

ur.__version__        # the installed distribution version
ur.__core_version__   # the native core version, or None if the extension is absent
```

Wheels bundle the compiled Rust core, so **no Rust toolchain is required** to install. Python ≥ 3.10.

## Optional dependencies

`polars` is optional but recommended. It is touched only by the interop shims — `.to_polars()` and
`ur.from_polars()` — and Ursa never depends on the Polars Rust crates. Everything crosses the
boundary as Arrow, so the interop is zero-copy regardless.

```bash
pip install ursa-graph polars
```

`pyarrow` is required for the Arrow egress paths (`.to_arrow()`, `sink_parquet`, `from_arrow`).

## Building from source

The Python side is managed with [uv](https://docs.astral.sh/uv/); the Rust toolchain is pinned in
`rust-toolchain.toml`, so rustup installs the exact version CI uses.

```bash
git clone https://github.com/cldixon/ursa
cd ursa

uv sync        # creates the venv, builds the maturin extension, installs dev deps
uv run pytest  # pure-Python tests + native-kernel tests
```

`uv run` rebuilds the native extension as needed, so editing Rust and re-running `uv run pytest`
picks the change up.

| Command | What it does |
|---|---|
| `cargo test -p ursa-core` | The kernels alone — fast, `arrow` + `rayon` only, no DataFusion |
| `cargo check` | The whole workspace; compiles DataFusion, so it is slower |
| `uv run pytest` | The Python suite, including the NetworkX cross-checks |
| `uv run ruff check .` | Lint |
| `uv run ruff format .` | Format |
| `uv run ty check` | Type-check |

## Type checking

The package ships a hand-written stub for the native extension and fully typed Python source. If
your type-checker reports `ursa` as untyped, check that you are on a release that includes the
PEP 561 `py.typed` marker.

## Verifying the install

```python
import ursa as ur
import pyarrow as pa

edges = ur.from_arrow(pa.table({"s": [0, 1, 2], "d": [1, 2, 0]}), src="s", dst="d")
print(ur.pagerank(edges).collect().to_dicts())
```

A three-node cycle: every node should come back with the same score.
