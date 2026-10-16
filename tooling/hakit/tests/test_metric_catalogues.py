"""Metric catalogues on the search path: the base declares the series the tooling pushes; a broken or repeated family is refused."""

from __future__ import annotations

import json
from pathlib import Path

import pytest

from hakit.content import ContentError, discover
from hakit.jsonval import Json
from hakit.paths import BASE_CONTENT
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()


def family(name: str, **extra: object) -> Json:
    return {"name": name, "kind": "counter", "semi_open": True, "labels": [[{"name": "endpoint", "values": []}]], "help": "h", **extra}


def catalogue(root: Path, name: str, families: list[Json]) -> Path:
    p = root / "metrics" / f"{name}.json"
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps({"catalogues": [{"binary": name, "families": families}]}))
    return p


def refusal(path: list[Path]) -> list[str]:
    with pytest.raises(ContentError) as e:
        discover(path, SCHEMAS)
    return e.value.errors


def test_the_base_declares_the_series_the_tooling_pushes() -> None:
    families = discover([BASE_CONTENT], SCHEMAS).metric_families
    for name in ("sip_chaos_event", "sip_chaos_window", "sip_chaos_window_verdict", "sip_chaos_run_verdict", "sipp_records_total"):
        assert families[name] == BASE_CONTENT / "metrics" / "hakit.json"


def test_a_product_catalogue_is_read_beside_the_base(tmp_path: Path) -> None:
    p = catalogue(tmp_path, "widget", [family("widget_requests_total")])
    assert discover([BASE_CONTENT, tmp_path], SCHEMAS).metric_families["widget_requests_total"] == p


def test_a_family_declared_twice_on_the_path_is_refused_naming_both_files(tmp_path: Path) -> None:
    p = catalogue(tmp_path, "widget", [family("sip_chaos_event", kind="gauge")])
    assert refusal([BASE_CONTENT, tmp_path]) == [
        f"metric family 'sip_chaos_event' is defined twice: {BASE_CONTENT / 'metrics' / 'hakit.json'} and {p}"
    ]


def test_a_malformed_family_is_refused_with_its_file(tmp_path: Path) -> None:
    p = catalogue(tmp_path, "widget", [family("widget_total", kind="summary", help=""), family("bad name")])
    assert refusal([tmp_path]) == [
        f"{p}: widget_total: kind 'summary' is none of counter, gauge, histogram",
        f"{p}: widget_total: no help text",
        f"{p}: family name 'bad name' is no metric name",
    ]


def test_a_file_without_catalogues_is_refused(tmp_path: Path) -> None:
    p = tmp_path / "metrics" / "empty.json"
    p.parent.mkdir(parents=True)
    p.write_text('{"families": []}')
    assert refusal([tmp_path]) == [f"{p}: catalogues: expected an array, got NoneType"]
