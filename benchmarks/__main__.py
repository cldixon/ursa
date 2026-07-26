"""Launcher so the CLI runs from the repo root without an install step:

    uv run --group bench python benchmarks --help
    uv run --group bench python benchmarks run -d er-1k -a pagerank

Running ``python benchmarks`` puts this directory on ``sys.path[0]``, which makes
the ``ursa_bench`` package importable regardless of the caller's cwd.
"""

from ursa_bench.cli import app

if __name__ == "__main__":
    app()
