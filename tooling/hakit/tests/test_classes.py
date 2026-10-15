"""Impact classes a product adds on the search path (ADR-0049 §3.10)."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from kit import T0, executable, write_yaml
from rundir import BASE_CONTENT, SCHEMAS, kill, run_dir

from hakit import jsonval as jv
from hakit.classes import ProductClass, read_counts, run_counts
from hakit.content import ContentError, discover
from hakit.executor import Call, FakeExecutor, ProcResult
from hakit.jsonval import Json
from hakit.verdict import compute


def product_class(root: Path, name: str, count: str | None = "echo '[]'") -> Path:
    p = write_yaml(root / "classes" / f"{name}.yaml", {"schema": "impact-class.v1", "name": name, "description": "d", "unit": "calls"})
    if count is not None:
        executable(root / "classes" / f"{name}.count", count)
    return p


def test_a_class_is_discovered_with_its_count_executable(tmp_path: Path) -> None:
    product_class(tmp_path, "media_gap")
    c = discover([BASE_CONTENT, tmp_path], SCHEMAS)
    assert c.classes["media_gap"] == ProductClass("media_gap", "calls", tmp_path / "classes" / "media_gap.count")


def test_a_class_without_its_count_is_refused(tmp_path: Path) -> None:
    product_class(tmp_path, "media_gap", count=None)
    with pytest.raises(ContentError, match="needs an executable media_gap.count"):
        discover([BASE_CONTENT, tmp_path], SCHEMAS)


def test_a_built_in_name_is_refused(tmp_path: Path) -> None:
    product_class(tmp_path, "cdr_mismatch")
    with pytest.raises(ContentError):
        discover([BASE_CONTENT, tmp_path], SCHEMAS)


def test_each_count_runs_once_over_the_run_directory_and_its_answer_is_kept(tmp_path: Path) -> None:
    run = tmp_path / "run"
    run.mkdir()
    out = {"ok": '[{"at_ms": 5, "call_id": "c1", "detail": "gap"}]', "bad": "not json", "fails": ""}

    def rule(call: Call) -> ProcResult | None:
        name = Path(call.argv[0]).stem
        assert call.env == {"HAKIT_RUN_DIR": str(run)}
        return ProcResult(3, "", "boom") if name == "fails" else ProcResult(0, out[name], "")

    classes = {n: ProductClass(n, "calls", tmp_path / f"{n}.count") for n in out}
    fake = FakeExecutor([rule])
    run_counts(classes, run, fake)
    assert len(fake.calls) == 3
    impacts, failed = read_counts(run)
    assert [(i.cls, i.call_id, i.at_ms, i.detail) for i in impacts] == [("ok", "c1", 5, "gap")]
    assert [f["detail"] for f in failed] == [
        "class bad: output is not an array of impacts: Expecting value: line 1 column 1 (char 0)",
        "class fails: exit 3: boom",
    ]
    assert json.loads((run / "classes" / "ok.json").read_text())["impacts"] == [{"at_ms": 5, "call_id": "c1", "detail": "gap"}]


def test_a_class_is_budgeted_per_window_and_unattributed_outside(tmp_path: Path) -> None:
    doc: Json = {"schema": "budget.v1", "name": "media", "description": "d", "classes": {"media_gap": {"max": 1}}}
    assert SCHEMAS.errors("budget.v1", doc) == []
    impacts = [{"at_ms": T0 + 101_000, "call_id": "a"}, {"at_ms": T0 + 102_000, "call_id": "b"}, {"at_ms": T0 + 400_000}]
    d = run_dir(
        tmp_path,
        [],
        calls=0,
        events=kill(1, T0 + 100_000, budget="media"),
        budgets=[doc],
        classes={"media_gap": {"name": "media_gap", "unit": "calls", "impacts": impacts}},
    )
    v = compute(d, SCHEMAS)
    assert SCHEMAS.errors("verdict.v1", v) == []
    (w,) = jv.objs(v, "windows")
    assert [(r["class"], r["count"], r["allowed"], r["finding"], r["call_ids"]) for r in jv.objs(w, "impacts")] == [
        ("media_gap", 2, 1.0, True, ["a", "b"])
    ]
    assert [(u["class"], u["count"], u["call_ids"]) for u in jv.objs(v, "unattributed")] == [("media_gap", 1, [])]
    assert v["status"] == "fail"


def test_a_failed_count_kept_in_the_run_directory_makes_the_run_invalid(tmp_path: Path) -> None:
    d = run_dir(tmp_path, [], calls=0, events=[], classes={"media_gap": {"name": "media_gap", "unit": "calls", "error": "exit 1: x"}})
    v = compute(d, SCHEMAS)
    assert [(i["kind"], i["detail"]) for i in jv.objs(v, "integrity")] == [("class_failed", "class media_gap: exit 1: x")]
    assert v["status"] == "invalid"
