"""Dashboards on the search path: each carries the generic block, uids are unique, and Grafana mounts every directory in place."""

from __future__ import annotations

import copy
import json
from pathlib import Path

import pytest
import yaml

from hakit.cli import main
from hakit.content import ContentError, discover
from hakit.dashboards import LINK, PHASE_ANNOTATION, TAG, WINDOW_ANNOTATION
from hakit.grafana import MOUNT_ROOT, compose_override
from hakit.jsonval import Json, as_list, as_obj
from hakit.paths import BASE_CONTENT
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()


def dashboard(root: Path, uid: str, **extra: object) -> Path:
    doc: Json = {
        "uid": uid,
        "title": uid,
        "tags": [TAG],
        "annotations": {"list": [copy.deepcopy(WINDOW_ANNOTATION), copy.deepcopy(PHASE_ANNOTATION)]},
        "links": [copy.deepcopy(LINK)],
        "panels": [],
        **extra,
    }
    p = root / "dashboards" / f"{uid}.json"
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(doc))
    return p


def refusal(path: list[Path]) -> list[str]:
    with pytest.raises(ContentError) as e:
        discover(path, SCHEMAS)
    return e.value.errors


def test_every_base_dashboard_carries_the_generic_block() -> None:
    content = discover([BASE_CONTENT], SCHEMAS)
    assert len(content.dashboards) == len(list((BASE_CONTENT / "dashboards").rglob("*.json")))
    assert {"sipp-endurance-chaos", "b2bua-worker", "loadgen-perf"} <= set(content.dashboards)
    assert content.dashboard_dirs == [(0, BASE_CONTENT / "dashboards")]


def test_a_product_dashboard_directory_is_mounted_in_place_after_the_base(tmp_path: Path) -> None:
    product = tmp_path / "product"
    p = dashboard(product, "product-rows")
    content = discover([BASE_CONTENT, product], SCHEMAS)
    assert content.dashboards["product-rows"] == p
    override = as_obj(yaml.safe_load(compose_override(content.dashboard_dirs)))
    volumes = as_list(as_obj(as_obj(override["services"])["grafana"])["volumes"])
    assert volumes == [
        {"type": "bind", "source": str(BASE_CONTENT / "dashboards"), "target": f"{MOUNT_ROOT}/0", "read_only": True},
        {"type": "bind", "source": str(product / "dashboards"), "target": f"{MOUNT_ROOT}/1", "read_only": True},
    ]


def test_a_mount_is_named_by_the_entry_s_position_on_the_path(tmp_path: Path) -> None:
    bare, product = tmp_path / "bare", tmp_path / "product"
    (bare / "budgets").mkdir(parents=True)
    dashboard(product, "product-rows")
    content = discover([BASE_CONTENT, bare, product], SCHEMAS)
    override = as_obj(yaml.safe_load(compose_override(content.dashboard_dirs)))
    volumes = as_list(as_obj(as_obj(override["services"])["grafana"])["volumes"])
    assert [as_obj(v)["target"] for v in volumes] == [f"{MOUNT_ROOT}/0", f"{MOUNT_ROOT}/2"]


def test_a_dashboard_in_a_subdirectory_is_read_like_grafana_reads_it(tmp_path: Path) -> None:
    nested = tmp_path / "dashboards" / "team"
    p = dashboard(nested.parent.parent, "x")
    moved = nested / "x.json"
    nested.mkdir(parents=True)
    p.rename(moved)
    assert discover([tmp_path], SCHEMAS).dashboards == {"x": moved}
    bare = nested / "bare.json"
    bare.write_text(json.dumps({"uid": "bare", "title": "bare"}))
    assert f"{bare}: no tag {TAG!r}" in refusal([tmp_path])


def test_a_title_defined_twice_on_the_path_is_refused(tmp_path: Path) -> None:
    first = dashboard(tmp_path / "a", "one", title="Same title")
    second = dashboard(tmp_path / "b", "two", title="Same title")
    assert refusal([tmp_path / "a", tmp_path / "b"]) == [
        f"dashboard title 'Same title' is defined twice: {first} and {second}"
    ]


def test_a_dashboard_without_the_generic_block_is_refused_naming_what_it_lacks(tmp_path: Path) -> None:
    p = dashboard(tmp_path, "bare", annotations={"list": [copy.deepcopy(WINDOW_ANNOTATION)]}, links=[], tags=[])
    assert refusal([tmp_path]) == [
        f"{p}: no 'Incident phase' annotation on {PHASE_ANNOTATION['expr']}",
        f"{p}: no tag {TAG!r}",
        f"{p}: no link to the dashboards tagged {TAG!r} with the time range kept",
    ]


def test_an_annotation_with_another_query_is_not_the_block(tmp_path: Path) -> None:
    window = copy.deepcopy(WINDOW_ANNOTATION)
    window["expr"] = "sip_chaos_window == 1"
    p = dashboard(tmp_path, "drifted", annotations={"list": [window, copy.deepcopy(PHASE_ANNOTATION)]})
    assert refusal([tmp_path]) == [f"{p}: no 'Incident window' annotation on {WINDOW_ANNOTATION['expr']}"]


def test_the_block_reads_the_victoriametrics_datasource(tmp_path: Path) -> None:
    phase = copy.deepcopy(PHASE_ANNOTATION)
    phase["datasource"] = {"type": "prometheus", "uid": "other"}
    p = dashboard(tmp_path, "elsewhere", annotations={"list": [copy.deepcopy(WINDOW_ANNOTATION), phase]})
    assert refusal([tmp_path]) == [f"{p}: the 'Incident phase' annotation reads another datasource than VictoriaMetrics (uid vm)"]


def test_a_uid_defined_twice_on_the_path_is_refused_naming_both_files(tmp_path: Path) -> None:
    p = dashboard(tmp_path, "sipp-endurance-chaos")
    base = BASE_CONTENT / "dashboards" / "sipp-endurance-chaos.json"
    assert refusal([BASE_CONTENT, tmp_path]) == [f"dashboard 'sipp-endurance-chaos' is defined twice: {base} and {p}"]


def test_the_dashboards_command_writes_the_override_of_the_whole_path(
    tmp_path: Path, monkeypatch: pytest.MonkeyPatch, capsys: pytest.CaptureFixture[str]
) -> None:
    product = tmp_path / "product"
    dashboard(product, "product-rows")
    monkeypatch.setenv("HAKIT_PATH", str(product))
    out = tmp_path / "gen" / "dashboards.compose.yml"
    assert main(["dashboards", "--compose-override", str(out)]) == 0
    listed = capsys.readouterr().out.splitlines()
    assert f"product-rows\t{product / 'dashboards' / 'product-rows.json'}" in listed
    targets = [v["target"] for v in yaml.safe_load(out.read_text())["services"]["grafana"]["volumes"]]
    assert targets == [f"{MOUNT_ROOT}/0", f"{MOUNT_ROOT}/1"]
