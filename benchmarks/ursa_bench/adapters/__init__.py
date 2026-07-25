"""Per-library adapters: ingest a Parquet edge list into a native graph, then run
a canonical algorithm on it, returning results keyed by the *original* node id.

Adapters are imported lazily (by :func:`get_adapter`) so a measurement child pays
the import cost of *only* the one library it is racing — which is what keeps the
peak-RSS reading honest.
"""

from __future__ import annotations

from .base import Adapter, load_edges, read_result, serialize_result

__all__ = [
    "Adapter",
    "get_adapter",
    "known_libraries",
    "load_edges",
    "read_result",
    "serialize_result",
]

# name -> "module:ClassName", resolved on demand.
_ADAPTERS = {
    "ursa": "ursa_bench.adapters.ursa_adapter:UrsaAdapter",
    "networkx": "ursa_bench.adapters.networkx_adapter:NetworkxAdapter",
    "rustworkx": "ursa_bench.adapters.rustworkx_adapter:RustworkxAdapter",
}


def known_libraries() -> list[str]:
    return list(_ADAPTERS)


def get_adapter(name: str) -> Adapter:
    import importlib

    try:
        spec = _ADAPTERS[name]
    except KeyError:
        raise KeyError(f"unknown library {name!r}; known: {', '.join(_ADAPTERS)}") from None
    mod_name, cls_name = spec.split(":")
    mod = importlib.import_module(mod_name)
    return getattr(mod, cls_name)()
