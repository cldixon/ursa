"""Every ```python block in the documentation runs, in page order, in a scratch
directory. A page's blocks share one namespace, so a later block can build on an
earlier one. Indented blocks inside content tabs count too. Blocks fenced as
```py are illustrative (object storage, placeholder paths) and are not run.

Bit-rot guard for the docs site: an example that stops running fails here, by
page and block number, before it ships. The committed ```text output blocks are
not compared — a float that moves in the last digit is not a doc failure — but
an example must still execute.
"""

from __future__ import annotations

import re
import textwrap
from pathlib import Path

import pytest

import ursa as ur

DOCS = Path(__file__).resolve().parent.parent / "docs"
_BLOCK = re.compile(
    r"^(?P<indent>[ \t]*)```python[^\n]*\n(?P<code>.*?)^(?P=indent)```", re.MULTILINE | re.DOTALL
)


def _pages() -> list[Path]:
    return sorted(p for p in DOCS.rglob("*.md") if _BLOCK.search(p.read_text(encoding="utf-8")))


def python_blocks(page: Path) -> list[str]:
    # Blocks inside content tabs are indented; dedent them so they compile.
    return [
        textwrap.dedent(m.group("code")) for m in _BLOCK.finditer(page.read_text(encoding="utf-8"))
    ]


@pytest.mark.skipif(not ur._NATIVE_AVAILABLE, reason="native extension not built (run `uv sync`)")
@pytest.mark.parametrize("page", _pages(), ids=lambda p: str(p.relative_to(DOCS)))
def test_docs_page_examples_run(page: Path, tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)  # sink_* examples write relative paths
    namespace: dict = {"__name__": f"docs.{page.stem}"}
    for i, block in enumerate(python_blocks(page), start=1):
        code = compile(block, f"{page.relative_to(DOCS)}#block-{i}", "exec")
        exec(code, namespace)
