# Install

Ursa is a Python package with a compiled Rust core. The wheels on PyPI include the core, so you do not need a Rust toolchain.

## Requirements

- Python 3.10 or later.
- Linux (x86_64, aarch64), macOS (x86_64, Apple silicon), or Windows (x64).

## Install the package

The package name on PyPI is `ursa-graph`. The import name is `ursa`.

=== "pip"

    ```bash
    pip install ursa-graph
    ```

=== "uv"

    ```bash
    uv add ursa-graph
    ```

!!! warning "Install `ursa-graph`, not `ursa`"

    The PyPI name `ursa` belongs to a different, abandoned project. If you install it, `import ursa` does not give you this library.

## Optional extras

Ursa moves data as Apache Arrow. `pyarrow` is the only required dependency. The extras below add one interop path each. Ursa imports the extra only when you call the matching function.

| Extra | Adds | Enables |
|---|---|---|
| `polars` | `polars` | `result.to_polars()` and `ur.from_polars(df, ...)` |
| `networkx` | `networkx` | `ur.from_networkx(G)` and `ur.nodes_from_networkx(G)` |
| `numpy` | `numpy` | `ur.from_numpy(array)` |
| `scipy` | `scipy` | `ur.from_scipy_sparse(matrix)` |
| `all` | all of the above | everything |

```bash
pip install 'ursa-graph[polars]'
pip install 'ursa-graph[all]'
```

`pandas` is not an extra. If `pandas` is already installed, `ur.from_pandas(df, ...)` and the frame constructors accept a `pandas.DataFrame`.

## Check the installation

```python
import ursa as ur

print(ur.__version__)        # the package version
print(ur.__core_version__)   # the Rust core version; the same number
```

If `ur.__core_version__` is `None`, the compiled core did not load. Make sure you installed a wheel for your platform and Python version.

## Install from source

A source install compiles the Rust core. You need a Rust toolchain (1.88 or later) and `maturin`.

```bash
git clone https://github.com/cldixon/ursa
cd ursa
uv sync          # builds the extension and installs the dev tools
uv run pytest    # runs the test suite
```

## Next step

Continue with the [quickstart](quickstart.md).
