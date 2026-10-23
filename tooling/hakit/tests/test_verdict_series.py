"""The verdict pushed as series when it is written: one verdict and one findings count per window, one verdict per run."""

from __future__ import annotations

import json
from pathlib import Path

from hakit import jsonval as jv
from hakit.content import discover
from hakit.dryrun import simulated
from hakit.executor import Call, HttpResult
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.runner import Runner
from hakit.schemas import SchemaSet
from hakit.verdict_series import verdict_lines

SCHEMAS = SchemaSet.load()


def window(event: str, findings: int, status: str = "ok") -> Json:
    return {"event": event, "budget": "b", "status": status, "findings": findings}


def test_each_window_and_the_run_become_series_at_the_verdict_instant() -> None:
    v: Json = {
        "run": "r1",
        "profile": "p",
        "status": "invalid",
        "windows": [window("001-kill", 0), window("002-kill", 3), window("003-freeze", 0, "incident_failed")],
    }
    assert verdict_lines(v, 5000) == [
        'sip_chaos_window_verdict{run="r1",event="001-kill",budget="b"} 0 5000',
        'sip_chaos_window_findings{run="r1",event="001-kill",budget="b"} 0 5000',
        'sip_chaos_window_verdict{run="r1",event="002-kill",budget="b"} 1 5000',
        'sip_chaos_window_findings{run="r1",event="002-kill",budget="b"} 3 5000',
        'sip_chaos_window_verdict{run="r1",event="003-freeze",budget="b"} 2 5000',
        'sip_chaos_window_findings{run="r1",event="003-freeze",budget="b"} 0 5000',
        'sip_chaos_run_verdict{run="r1",profile="p"} 2 5000',
    ]


def test_a_verdict_push_that_fails_is_said_and_noted_in_the_summary(tmp_path: Path) -> None:
    def refuse_verdicts(c: Call) -> HttpResult | None:
        if c.kind == "http" and c.body and b"sip_chaos_run_verdict" in c.body:
            return HttpResult(503, "busy")
        return None

    content = discover([BASE_CONTENT], SCHEMAS)
    clock, fake = simulated([refuse_verdicts])
    said: list[str] = []
    run_dir = Runner(content, SCHEMAS, lab_from_env({}), fake, clock, tmp_path, say=said.append).run("kill-worker-smoke")
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "pass"
    assert any("verdict series did not reach VictoriaMetrics" in s for s in said), said
    assert "verdict series did not reach VictoriaMetrics" in (run_dir / "summary.md").read_text()


def test_a_dry_run_pushes_its_verdict(tmp_path: Path) -> None:
    content = discover([BASE_CONTENT], SCHEMAS)
    clock, fake = simulated()
    run_dir = Runner(content, SCHEMAS, lab_from_env({}), fake, clock, tmp_path, say=lambda _m: None).run("kill-worker-smoke")
    pushed = [ln for c in fake.calls if c.kind == "http" and c.body for ln in c.body.decode().splitlines()]
    verdicts = [ln for ln in pushed if ln.startswith("sip_chaos_run_verdict")]
    assert verdicts == [f'sip_chaos_run_verdict{{run="{run_dir.name}",profile="kill-worker-smoke"}} 0 {clock.now_ms()}']
    windows = jv.objs(as_obj(json.loads((run_dir / "verdict.json").read_text())), "windows")
    assert windows and sum(ln.startswith("sip_chaos_window_verdict") for ln in pushed) == len(windows)
