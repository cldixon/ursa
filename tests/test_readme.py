"""The README is the first thing a new user reads, and the only page GitHub and
PyPI both render. These guard the two things that made it unusable before (#121):
the install instructions naming the real distribution, and a first example that
actually runs.
"""

import re
from pathlib import Path

import pytest

import ursa as ur

README = Path(__file__).resolve().parent.parent / "README.md"


def test_readme_names_the_distribution_and_import_name():
    text = README.read_text(encoding="utf-8")
    assert "pip install ursa-graph" in text, "README must say how to install"
    assert "uv add ursa-graph" in text
    # The distribution/import split is the trap: `pip install ursa` is a different,
    # unrelated project on PyPI.
    assert "`ursa-graph`" in text and "`ursa`" in text


def test_readme_documents_the_polars_extra():
    assert "ursa-graph[polars]" in README.read_text(encoding="utf-8")


def _try_it_snippet() -> str:
    """The python block under `## Try it` — the first code a new user pastes."""
    text = README.read_text(encoding="utf-8")
    section = text.split("## Try it", 1)
    assert len(section) == 2, "README lost its `## Try it` section"
    match = re.search(r"```python\n(.*?)```", section[1], re.DOTALL)
    assert match, "`## Try it` must contain a python block"
    return match.group(1)


@pytest.mark.skipif(not ur._NATIVE_AVAILABLE, reason="native extension not built (run `uv sync`)")
def test_readme_try_it_snippet_runs(capsys):
    """Bit-rot guard: the snippet is promised to need no files and no network."""
    snippet = _try_it_snippet()
    assert "ur.datasets.load_karate()" in snippet, "the first example must need no files"

    exec(compile(snippet, "README.md#try-it", "exec"), {"__name__": "__readme__"})

    out = capsys.readouterr().out
    assert "shape: (5," in out, "the snippet prints a 5-row preview"
    assert "pagerank" in out
