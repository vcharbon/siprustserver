"""The SIPp injection file rendered from a catalog shape."""

from __future__ import annotations

import pytest

from hakit.injection import InjectionError, render
from hakit.model import Catalog, ExpectedEnding, Shape


def shape(headers: list[tuple[str, str]], ruri: str | None = None) -> Shape:
    return Shape("s", "c", ruri, headers, [], [ExpectedEnding("answered", None, None)], True)


def catalog(placeholders: dict[str, str] | None = None) -> Catalog:
    return Catalog("c", placeholders or {"callee_host": "h", "callee_port": "p"}, [], raw={})


def test_one_row_per_callee_with_the_placeholders_filled() -> None:
    s = shape([("X-Api-Call", '{"destination":{"host":"${callee_host}","port":${callee_port}}}')])
    assert render(s, catalog(), [("10.0.0.1", 5060), ("10.0.0.2", 5070)]) == (
        "SEQUENTIAL\n"
        'service;X-Api-Call: {"destination":{"host":"10.0.0.1","port":5060}};;;\n'
        'service;X-Api-Call: {"destination":{"host":"10.0.0.2","port":5070}};;;\n'
    )


def test_the_ruri_user_comes_from_the_shape() -> None:
    assert render(shape([], "+3312"), catalog(), [("h", 1)]) == "SEQUENTIAL\n+3312;;;;\n"


def test_a_placeholder_the_renderer_does_not_fill_is_refused() -> None:
    with pytest.raises(InjectionError, match="does not fill"):
        render(shape([]), catalog({"tenant": "t"}), [("h", 1)])
    with pytest.raises(InjectionError, match=r"\$\{tenant\}"):
        render(shape([("X", "${tenant}")]), catalog(), [("h", 1)])


def test_a_field_separator_or_too_many_headers_is_refused() -> None:
    with pytest.raises(InjectionError, match="field separator"):
        render(shape([("X", "a;b")]), catalog(), [("h", 1)])
    with pytest.raises(InjectionError, match="5 headers"):
        render(shape([("X", "1")] * 5), catalog(), [("h", 1)])
