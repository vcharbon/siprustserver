"""The SIPp injection file rendered from a catalog shape."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from hakit.cli import main
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


def test_hakit_inject_renders_a_product_catalog_on_the_search_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    """A product's catalog (here one asking by dialed number) is rendered like the base's."""
    doc = {
        "schema": "treatment-catalog.v1",
        "name": "dialed",
        "shapes": [
            {
                "shape": "dialed_em",
                "description": "d",
                "request": {"ruri_user": "+15550100", "headers": [{"name": "Resource-Priority", "value": "esnet.0"}]},
                "expected": [{"ending": "answered"}],
                "load_safe": True,
            }
        ],
    }
    (tmp_path / "catalogs").mkdir()
    (tmp_path / "catalogs" / "dialed.json").write_text(json.dumps(doc))
    monkeypatch.setenv("HAKIT_PATH", str(tmp_path))
    assert main(["inject", "dialed_em", "--callee", "10.0.0.1"]) == 0
    assert capsys.readouterr().out == "SEQUENTIAL\n+15550100;Resource-Priority: esnet.0;;;\n"
    assert main(["inject", "basic", "--callee", "10.0.0.1:5070"]) == 0
    assert '"host":"10.0.0.1","port":5070' in capsys.readouterr().out
    assert main(["inject", "absent"]) == 2
