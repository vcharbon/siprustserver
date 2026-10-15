"""The smoke profile on the simulated lab when the lab misbehaves: a slow probe,
a generator that does not drain, a refused push, a signal, a silent VictoriaMetrics."""

from __future__ import annotations

import json
import os
import signal
from pathlib import Path

import pytest

from hakit import jsonval as jv
from hakit.content import discover
from hakit.dryrun import simulated
from hakit.executor import Call, FakeExecutor, HttpResult, ProcResult, Rule
from hakit.jsonval import as_obj
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.runner import Runner, RunRefused
from hakit.schemas import SchemaSet
from hakit.verdict import read_jsonl

SCHEMAS = SchemaSet.load()
SMOKE = "kill-worker-smoke"


def runner(tmp_path: Path, rules: list[Rule]) -> tuple[Runner, FakeExecutor]:
    clock, fake = simulated(rules)
    content = discover([BASE_CONTENT], SCHEMAS)
    return Runner(content, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None), fake


def is_step(c: Call, step: str) -> bool:
    return c.kind == "run" and c.argv[0].endswith(f"kill_worker/{step}")


def test_the_kill_worker_probe_is_retried_until_it_exits_0(tmp_path: Path) -> None:
    seen = {"n": 0}

    def slow_probe(c: Call) -> ProcResult | None:
        if is_step(c, "probe"):
            seen["n"] += 1
            return ProcResult(0, '{"ready_at_ms":1}\n', "") if seen["n"] % 3 == 0 else ProcResult(75, "", "")
        return None

    r, fake = runner(tmp_path, [slow_probe])
    run_dir = r.run(SMOKE)
    lines = [e for e in read_jsonl(run_dir / "events.jsonl") if e.get("event") == "001-kill_worker" and e.get("kind") == "phase"]
    healed = next(int(str(e["at_ms"])) for e in lines if e["phase"] == "healed")
    recovered = next(int(str(e["at_ms"])) for e in lines if e["phase"] == "recovered")
    assert recovered - healed == 2 * 2000  # two not-yet answers, 2 s apart
    assert len([c for c in fake.calls if is_step(c, "probe")]) == 4 * 3
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "pass"


def test_a_generator_that_does_not_drain_is_killed(tmp_path: Path) -> None:
    def never_exits(c: Call) -> ProcResult | None:
        if c.kind == "run" and c.argv[:2] == ["docker", "inspect"] and "Running" in c.argv[3]:
            return ProcResult(0, "true\n", "")
        return None

    r, fake = runner(tmp_path, [never_exits])
    run_dir = r.run(SMOKE)
    assert ["docker", "kill", "hakit-basic"] in [c.argv for c in fake.calls]
    (stream,) = json.loads((run_dir / "streams.json").read_text())
    assert stream["drained"] is False


def test_refused_pushes_are_counted_in_the_window_facts(tmp_path: Path) -> None:
    def refuse(c: Call) -> HttpResult | None:
        return HttpResult(500, "no") if c.kind == "http" and "import/prometheus" in c.url else None

    r, _ = runner(tmp_path, [refuse])
    v = as_obj(json.loads((r.run(SMOKE) / "verdict.json").read_text()))
    w = jv.objs(v, "windows")[0]
    failed = jv.as_obj(jv.as_obj(jv.obj(w, "facts")["hakit"])["vm_push"])["failed"]
    assert isinstance(failed, int) and failed >= 6  # five phases and the close, at least


def test_a_termination_signal_heals_the_open_event(tmp_path: Path) -> None:
    def terminate(c: Call) -> ProcResult | None:
        if is_step(c, "inject"):
            os.kill(os.getpid(), signal.SIGTERM)
        return None

    before = signal.getsignal(signal.SIGTERM)
    r, fake = runner(tmp_path, [terminate])
    with pytest.raises(SystemExit):
        r.run(SMOKE)
    assert any(is_step(c, "heal") for c in fake.calls)
    assert fake.calls[-1].argv[:3] == ["docker", "rm", "-f"]
    assert signal.getsignal(signal.SIGTERM) == before


def test_a_silent_victoriametrics_refuses_the_run(tmp_path: Path) -> None:
    def silent(c: Call) -> HttpResult | None:
        return HttpResult(0, "connection refused") if c.kind == "http" and "/api/v1/query?" in c.url else None

    r, _ = runner(tmp_path, [silent])
    with pytest.raises(RunRefused, match="leftover_calls: VictoriaMetrics did not answer"):
        r.run(SMOKE)
