"""The materialized-result surface a new user meets first: the repr, the row
count, and the optional-dependency error.

None of this needs the native extension — a ``MaterializedFrame`` wraps an Arrow
table directly — so these run in every environment.
"""

import builtins

import pytest

import ursa as ur

pa = pytest.importorskip("pyarrow")


def frame(**columns) -> ur.MaterializedFrame:
    return ur.MaterializedFrame(pa.table(columns))


# --- repr (#123) ------------------------------------------------------------


def test_repr_shows_shape_names_dtypes_and_values():
    text = repr(frame(id=pa.array([0, 1], pa.int64()), pr=pa.array([0.25, 0.75], pa.float64())))
    assert "shape: (2, 2)" in text
    assert "id" in text and "pr" in text
    assert "int64" in text and "double" in text
    # The actual data — the whole point of the change.
    assert "0.25" in text and "0.75" in text


def test_repr_elides_beyond_the_row_cap():
    text = repr(frame(id=pa.array(list(range(50)), pa.int64())))
    assert "shape: (50, 1)" in text
    assert "…" in text, "rows past the cap should elide"
    # Capped: the 11th row onwards is not rendered.
    assert "\n│ 9 " in text or "│ 9  " in text
    assert "42" not in text


def test_repr_of_short_frame_has_no_ellipsis_row():
    assert "…" not in repr(frame(id=pa.array([0, 1, 2], pa.int64())))


def test_repr_size_is_independent_of_row_count():
    """A repr fires on every REPL evaluation, so it must not scale with row count."""
    small = repr(frame(id=pa.array(list(range(11)), pa.int64())))
    big = repr(frame(id=pa.array(list(range(200_000)), pa.int64())))
    assert "shape: (200000, 1)" in big
    assert big.count("\n") == small.count("\n"), "both are capped at the same height"
    assert big.count("\n") < 20


def test_repr_renders_nulls_and_clips_long_strings():
    text = repr(
        frame(
            a=pa.array([None, 1], pa.int64()),
            b=pa.array(["x" * 100, "short"], pa.string()),
        )
    )
    assert "null" in text, "nulls should read as null, not None"
    assert "x" * 100 not in text, "a long cell should be clipped"
    assert "…" in text


def test_repr_of_empty_frame():
    text = repr(frame(id=pa.array([], pa.int64())))
    assert "shape: (0, 1)" in text


def test_repr_of_zero_column_frame():
    assert "shape: (0, 0)" in repr(ur.MaterializedFrame(pa.table({})))


def test_repr_never_raises(monkeypatch):
    """A repr that throws is worse than a terse one — the fallback must hold."""
    result = frame(id=pa.array([0], pa.int64()))
    monkeypatch.setattr(ur.MaterializedFrame, "_render", lambda self: 1 / 0, raising=True)
    assert "MaterializedFrame" in repr(result)


# --- row count / columns (#126) ---------------------------------------------


def test_len_and_columns():
    result = frame(id=pa.array([0, 1, 2], pa.int64()), v=pa.array([1, 2, 3], pa.int64()))
    assert len(result) == 3
    assert result.columns == ["id", "v"]


# --- the polars extra (#122) ------------------------------------------------


def test_to_polars_error_names_the_extra(monkeypatch):
    real_import = builtins.__import__

    def blocked(name, *args, **kwargs):
        if name == "polars":
            raise ModuleNotFoundError("No module named 'polars'")
        return real_import(name, *args, **kwargs)

    monkeypatch.setattr(builtins, "__import__", blocked)

    with pytest.raises(ModuleNotFoundError) as excinfo:
        frame(id=pa.array([0], pa.int64())).to_polars()

    message = str(excinfo.value)
    assert "ursa-graph[polars]" in message, "the error must name the installable extra"
    assert "to_polars" in message


def test_to_polars_still_works_when_polars_is_present():
    pytest.importorskip("polars")
    df = frame(id=pa.array([0, 1], pa.int64())).to_polars()
    assert df.shape == (2, 1)


# --- namespace hygiene (#126) -----------------------------------------------


@pytest.mark.parametrize("leaked", ["version", "ModuleType", "PackageNotFoundError"])
def test_import_machinery_is_not_exposed_as_api(leaked):
    """`ursa.version` in particular reads like a public accessor; it was importlib's."""
    assert not hasattr(ur, leaked), f"ursa.{leaked} leaks into the public namespace"


def test_version_is_still_reachable_under_its_dunder():
    assert isinstance(ur.__version__, str)
