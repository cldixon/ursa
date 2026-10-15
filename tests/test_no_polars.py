"""The first code a new user copies must run on a bare ``pip install ursa-graph``.

``polars`` is an optional extra (``ursa-graph[polars]``), so an example that ends in
``.to_polars()`` fails with ``ModuleNotFoundError`` for anyone who installed only
the base package. These run each first-contact example — the README's `## Try it`,
the ``ursa`` and ``ursa.datasets`` module docstrings (what ``help()`` shows), the
docs landing page and quickstart, and ``examples/quickstart.py`` — in a fresh
interpreter where ``import polars`` and ``import pandas`` are blocked.

A subprocess, not a monkeypatched ``sys.modules``: pyarrow caches whether pandas
imports, so blocking it in-process would leak into every later test.
"""

from __future__ import annotations

import re
import subprocess
import sys
import textwrap
from pathlib import Path

import pytest

import ursa as ur

ROOT = Path(__file__).resolve().parent.parent

pytestmark = pytest.mark.skipif(
    not ur._NATIVE_AVAILABLE, reason="native extension not built (run `uv sync`)"
)

# Make the optional dataframe libraries unimportable, as on a bare install: a
# meta-path finder that raises ModuleNotFoundError exactly as a missing package does
# (a `sys.modules[name] = None` stub confuses pyarrow's lazy pandas probe instead).
_PRELUDE = """\
import sys

class _NotInstalled:
    def find_spec(self, name, path=None, target=None):
        if name.partition(".")[0] in ("polars", "pandas"):
            raise ModuleNotFoundError(f"No module named {name!r}", name=name)
        return None

sys.meta_path.insert(0, _NotInstalled())
"""

_FENCED = re.compile(
    r"^(?P<indent>[ \t]*)```python[^\n]*\n(?P<code>.*?)^(?P=indent)```", re.MULTILINE | re.DOTALL
)


def _run_bare(code: str, cwd: Path) -> str:
    proc = subprocess.run(
        [sys.executable, "-c", _PRELUDE + code],
        cwd=cwd,
        capture_output=True,
        text=True,
        timeout=300,
    )
    assert proc.returncode == 0, f"failed without polars/pandas:\n{proc.stderr[-3000:]}"
    return proc.stdout


def _docstring_example(doc: str | None) -> str:
    """The first indented code block (starting at ``import ursa as ur``) in a docstring."""
    assert doc
    lines = doc.splitlines()
    start = next(i for i, line in enumerate(lines) if line.strip() == "import ursa as ur")
    block = []
    for line in lines[start:]:
        if line.strip() and not line.startswith("    "):
            break
        block.append(line)
    return textwrap.dedent("\n".join(block))


def _markdown_blocks(path: Path) -> str:
    text = path.read_text(encoding="utf-8")
    return "\n".join(textwrap.dedent(m.group("code")) for m in _FENCED.finditer(text))


def _readme_try_it() -> str:
    section = (ROOT / "README.md").read_text(encoding="utf-8").split("## Try it", 1)[1]
    match = re.search(r"```python\n(.*?)```", section, re.DOTALL)
    assert match
    return match.group(1)


def test_readme_try_it_runs_without_polars(tmp_path):
    out = _run_bare(_readme_try_it(), tmp_path)
    assert "shape: (5," in out


@pytest.mark.parametrize("module", ["ursa", "ursa.datasets"])
def test_module_docstring_example_runs_without_polars(module, tmp_path):
    __import__(module)
    example = _docstring_example(sys.modules[module].__doc__)
    assert "to_polars" not in example, f"`help({module})` example needs the polars extra"
    _run_bare(example, tmp_path)


@pytest.mark.parametrize("page", ["index.md", "quickstart.md"])
def test_first_contact_docs_run_without_polars(page, tmp_path):
    _run_bare(_markdown_blocks(ROOT / "docs" / page), tmp_path)


def test_example_script_runs_without_polars(tmp_path):
    script = ROOT / "examples" / "quickstart.py"
    out = _run_bare(f"import runpy; runpy.run_path({str(script)!r}, run_name='__main__')", tmp_path)
    assert "== expression dialect ==" in out


# The one labelled interop example. Everywhere else in the docs, README and examples
# uses polars-free egress, so nothing implies `.to_polars()` works on a bare install.
_POLARS_EXAMPLE_PAGE = "docs/guides/output.md"


def test_to_polars_appears_in_exactly_one_docs_example():
    pages = [*sorted((ROOT / "docs").rglob("*.md")), ROOT / "README.md"]
    using = sorted(str(p.relative_to(ROOT)) for p in pages if ".to_polars()" in _markdown_blocks(p))
    assert using == [_POLARS_EXAMPLE_PAGE], using
    assert "ursa-graph[polars]" in (ROOT / _POLARS_EXAMPLE_PAGE).read_text(encoding="utf-8")
    assert ".to_polars()" not in (ROOT / "examples" / "quickstart.py").read_text(encoding="utf-8")
