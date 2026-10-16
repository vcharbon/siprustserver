"""The runner's `start: incident` streams (ADR-0049 §2.8): a load incident
starts its stream at inject, drains it at heal without ever killing its UAC
(or kills it under `stop: abandon`), waits for its records, and each firing is
its own generator."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from kit import budget, write_yaml

from hakit.check import profile_errors
from hakit.content import Content, discover
from hakit.dryrun import STAT_HEADER, simulated
from hakit.executor import Call, FakeExecutor, HttpResult, ProcResult, Rule
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.model import Incident
from hakit.paths import BASE_CONTENT
from hakit.profile import parse_profile
from hakit.runner import Runner
from hakit.schedule import plan
from hakit.schemas import SchemaSet
from hakit.streams import RemoveLoad
from hakit.jsonl import read_jsonl

SCHEMAS = SchemaSet.load()
SOFT = ["docker", "kill", "--signal", "USR1", "hakit-burst"]
HARD = ["docker", "kill", "hakit-burst"]


def stream(name: str, start: str = "run", hold_s: float = 2) -> Json:
    return {
        "name": name,
        "driver": "sipp",
        "start": start,
        "shape": "basic",
        "scenario": "uac-basic-hold.xml",
        "cps": 50 if start == "incident" else 5,
        "setup_deadline_s": 10,
        "ring_s": 5,
        "hold_s": hold_s,
    }


def profile_doc(events: list[Json], streams: list[Json] | None = None) -> Json:
    return {
        "schema": "profile.v1",
        "name": "load",
        "description": "d",
        "duration_s": 600,
        "drain_s": 130,
        "settle_s": 15,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": streams or [stream("basic", hold_s=20), stream("burst", "incident")],
        "schedule": {"at": events},
    }


def content(tmp_path: Path, events: list[Json], streams: list[Json] | None = None) -> Content:
    root = tmp_path / "entry"
    budget(root, "any")
    write_yaml(root / "profiles" / "load.yaml", profile_doc(events, streams))
    return discover([BASE_CONTENT, root], SCHEMAS)


SPIKE: Json = {"at_s": 30, "incident": "spike", "stream": "burst", "budget": "any"}


def run(tmp_path: Path, rules: list[Rule], events: list[Json] | None = None) -> tuple[Path, FakeExecutor]:
    clock, fake = simulated(rules)
    c = content(tmp_path, events or [SPIKE])
    r = Runner(c, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    return r.run("load"), fake


def phase_at(run_dir: Path, event: str) -> dict[str, int]:
    return {
        str(e["phase"]): int(str(e["at_ms"]))
        for e in read_jsonl(run_dir / "events.jsonl")
        if e.get("kind") == "phase" and e.get("event") == event
    }


def is_burst_running(c: Call) -> bool:
    return c.kind == "run" and c.argv[:2] == ["docker", "inspect"] and "Running" in c.argv[3] and c.argv[-1] == "hakit-burst"


def test_a_drained_spike_heals_when_its_last_call_ends_and_never_kills_its_uac(tmp_path: Path) -> None:
    seen = {"after_usr1": 0, "usr1": False}

    def slow_drain(c: Call) -> ProcResult | None:
        if c.kind == "run" and c.argv == SOFT:
            seen["usr1"] = True
        if is_burst_running(c) and seen["usr1"]:
            seen["after_usr1"] += 1
            return ProcResult(0, "true\n" if seen["after_usr1"] <= 3 else "false\n", "")
        return None

    run_dir, fake = run(tmp_path, [slow_drain])
    at = phase_at(run_dir, "001-spike")
    assert at["healed"] - at["heal"] == 3 * 2000  # three polls still running, 2 s apart
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    assert HARD not in argvs
    rm = max(i for i, a in enumerate(argvs) if a[:3] == ["docker", "rm", "-f"] and "hakit-burst" in a)
    assert argvs.index(SOFT) < rm
    assert as_obj(json.loads((run_dir / "verdict.json").read_text()))["status"] == "pass"


def test_a_spike_that_does_not_drain_within_heal_s_is_killed_and_invalidates_the_run(tmp_path: Path) -> None:
    def never(c: Call) -> ProcResult | None:
        return ProcResult(0, "true\n", "") if is_burst_running(c) else None

    run_dir, fake = run(tmp_path, [never])
    at = phase_at(run_dir, "001-spike")
    assert "failed" in at and "healed" not in at
    assert at["failed"] - at["heal"] >= 300_000  # the spike's heal_s
    assert HARD in [c.argv for c in fake.calls if c.kind == "run"]
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "invalid"
    assert [i["kind"] for i in v["integrity"]] == ["incident_failed"]  # type: ignore[index]


def test_an_abandoned_stream_is_killed_and_left_out_of_the_reconciliation(tmp_path: Path) -> None:
    orphan: Json = {"at_s": 30, "incident": "orphan", "stream": "burst", "budget": "any"}

    def counted(c: Call) -> str | None:
        if c.kind == "read" and c.path is not None and c.path.name == "stat.csv" and "burst" in str(c.path):
            return STAT_HEADER + "0;0;0;5;9\n"
        return None

    run_dir, fake = run(tmp_path, [counted], [orphan])
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    assert HARD in argvs and SOFT not in argvs
    docs = {str(d["stream"]): d for d in json.loads((run_dir / "streams.json").read_text())}
    assert docs["burst"]["reconciled"] is False and docs["burst"]["calls"] == 9
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "pass"  # 9 calls and no record: not a record gap for an abandoned stream


def test_each_firing_is_its_own_generator_and_the_stream_sums_them(tmp_path: Path) -> None:
    second: Json = {**SPIKE, "at_s": 300}

    def counted(c: Call) -> str | None:
        if c.kind == "read" and c.path is not None and c.path.name == "stat.csv" and "burst" in str(c.path):
            return STAT_HEADER + "0;0;0;0;7\n"
        return None

    run_dir, fake = run(tmp_path, [counted], [SPIKE, second])
    uacs = [c.argv for c in fake.calls if c.kind == "run" and c.argv[:2] == ["docker", "run"] and "sipp" in c.argv and "hakit-burst" in c.argv]
    tags = [a[a.index("-cid_str") + 1] for a in uacs]
    assert tags == [f"%u-%p-{run_dir.name}-001-spike@%s", f"%u-%p-{run_dir.name}-002-spike@%s"]
    assert (run_dir / "streams" / "burst" / "001-spike" / "inject.csv").is_file()
    assert (run_dir / "streams" / "burst" / "002-spike" / "inject.csv").is_file()
    docs = {str(d["stream"]): d for d in json.loads((run_dir / "streams.json").read_text())}
    assert (docs["burst"]["firings"], docs["burst"]["calls"]) == (2, 14)
    v = as_obj(json.loads((run_dir / "verdict.json").read_text()))
    assert v["status"] == "invalid"  # 14 calls counted, no record: a record gap
    assert any("14 calls" in str(as_obj(i)["detail"]) for i in v["integrity"])  # type: ignore[union-attr]


def test_every_twin_keeps_the_error_entries_worth_reading_for_the_whole_run(tmp_path: Path) -> None:
    _, fake = run(tmp_path, [])
    runs = [c.argv for c in fake.calls if c.kind == "run" and c.argv[:2] == ["docker", "run"]]
    twins = [a for a in runs if a[a.index("--name") + 1].endswith("-exporter")]
    assert len(twins) == 2
    for t in twins:
        assert "SIPP_ERROR_KEPT_FILE=/stats/errors.kept.log" in t and f"SIPP_ERROR_KEPT_BYTES={256 << 20}" in t


def test_heal_ships_the_firings_records_before_removing_it_and_healed_is_its_exit(tmp_path: Path) -> None:
    seen = {"counts": 0}

    def records(c: Call) -> str | HttpResult | None:
        if c.kind == "read" and c.path is not None and c.path.name == "records.jsonl" and "burst" in str(c.path):
            return '{"r":1}\n{"r":2}\n'
        if c.kind == "http" and "stats" in c.url and "burst" in c.url:
            seen["counts"] += 1
            return HttpResult(200, '{"n":"%d"}\n' % (0 if seen["counts"] < 3 else 2))
        return None

    run_dir, fake = run(tmp_path, [records])
    at = phase_at(run_dir, "001-spike")
    heal_facts = next(
        as_obj(e["facts"]) for e in read_jsonl(run_dir / "events.jsonl") if e.get("kind") == "facts" and e.get("executable") == "heal"
    )
    assert at["healed"] == heal_facts["exited_at_ms"] == at["heal"]  # the simulated UAC exits at the signal
    assert (heal_facts["records"], heal_facts["shipped_records"]) == (2, 2)
    calls = fake.calls
    usr1 = next(i for i, c in enumerate(calls) if c.kind == "run" and c.argv == SOFT)
    rm = next(i for i, c in enumerate(calls) if i > usr1 and c.kind == "run" and c.argv[:3] == ["docker", "rm", "-f"] and "hakit-burst" in c.argv)
    between = calls[usr1:rm]
    assert any(c.kind == "read" and c.path is not None and c.path.name == "records.jsonl" for c in between)
    assert sum(1 for c in between if c.kind == "http" and "stats" in c.url and "burst" in c.url) == 3


def test_a_refused_drain_signal_fails_the_heal_once_and_kills_the_uac(tmp_path: Path) -> None:
    def refuse(c: Call) -> ProcResult | None:
        return ProcResult(1, "", "no such container") if c.kind == "run" and c.argv == SOFT else None

    run_dir, fake = run(tmp_path, [refuse])
    at = phase_at(run_dir, "001-spike")
    assert "failed" in at and "healed" not in at
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    assert argvs.count(SOFT) == 1 and HARD in argvs
    assert at["failed"] - at["heal"] < 120_000  # failed fast, not at heal_s


def test_streams_json_carries_each_streams_call_limit(tmp_path: Path) -> None:
    run_dir, _ = run(tmp_path, [])
    docs = {str(d["stream"]): d for d in json.loads((run_dir / "streams.json").read_text())}
    assert isinstance(docs["basic"]["call_limit"], int) and isinstance(docs["burst"]["call_limit"], int)


def test_an_incident_stream_that_exits_before_its_heal_is_a_stream_died(tmp_path: Path) -> None:
    seen = {"n": 0}

    def dies(c: Call) -> ProcResult | None:
        if is_burst_running(c):
            seen["n"] += 1
            return ProcResult(0, "true\n" if seen["n"] < 2 else "false\n", "")
        return None

    run_dir, _ = run(tmp_path, [dies])
    integrity = read_jsonl(run_dir / "integrity.jsonl")
    assert [(i["kind"], i["stream"]) for i in integrity] == [("stream_died", "burst")]


def test_an_interrupted_run_removes_the_running_burst_and_journals_its_heal(tmp_path: Path) -> None:
    class Stop(Exception):
        pass

    def interrupt(c: Call) -> ProcResult | None:
        if is_burst_running(c):
            raise Stop()
        return None

    clock, fake = simulated([interrupt])
    c = content(tmp_path, [SPIKE])
    r = Runner(c, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    with pytest.raises(Stop):
        r.run("load")
    run_dir = next((tmp_path / "results").iterdir())
    assert list(phase_at(run_dir, "001-spike")) == ["inject", "injected", "heal", "healed"]
    argvs = [c.argv for c in fake.calls if c.kind == "run"]
    assert SOFT not in argvs and ["docker", "rm", "-f", "hakit-burst", "hakit-burst-exporter"] in argvs


def test_cleanup_removes_a_crashed_runs_stream_by_its_labels(tmp_path: Path) -> None:
    c = content(tmp_path, [SPIKE])
    ev = plan(c.profiles["load"], c.incidents)[0]

    def listed(call: Call) -> ProcResult | None:
        return ProcResult(0, "abc\ndef\n", "") if call.argv[:3] == ["docker", "ps", "-aq"] else None

    fake = FakeExecutor([listed])
    ok, facts = RemoveLoad(fake, "run-1").stop(c.incidents["spike"], ev, interrupted=True)
    assert ok and facts["removed"] == 2
    assert fake.calls[0].argv == ["docker", "ps", "-aq", "--filter", "label=hakit-run=run-1", "--filter", "label=hakit-stream=burst"]
    assert fake.calls[1].argv == ["docker", "rm", "-f", "abc", "def"]


def errors_of(tmp_path: Path, events: list[Json], streams: list[Json] | None = None, extra: Incident | None = None) -> list[str]:
    c = content(tmp_path, [SPIKE])
    p = parse_profile(profile_doc(events, streams))
    if extra is not None:
        c.incidents[extra.name] = extra
    return profile_errors(p, c)


def test_a_load_event_needs_a_start_incident_stream(tmp_path: Path) -> None:
    errs = errors_of(tmp_path, [{**SPIKE, "stream": "basic"}])
    assert any("needs `stream` naming a `start: incident` stream" in e for e in errs)


def test_a_stream_named_by_an_event_without_a_load_incident_is_refused(tmp_path: Path) -> None:
    errs = errors_of(tmp_path, [{"at_s": 30, "incident": "kill_worker", "stream": "burst", "budget": "any"}])
    assert any("is set but the event fires no load incident" in e for e in errs)


def test_a_drain_longer_than_the_incidents_heal_s_is_refused(tmp_path: Path) -> None:
    slow = stream("burst", "incident", hold_s=400)
    errs = errors_of(tmp_path, [SPIKE], [stream("basic", hold_s=20), slow])
    assert any("needs up to 464 s to drain, above the heal_s 300 of 'spike'" in e for e in errs)


def test_one_stream_stopped_both_ways_is_refused(tmp_path: Path) -> None:
    orphan: Json = {"at_s": 400, "incident": "orphan", "stream": "burst", "budget": "any"}
    errs = errors_of(tmp_path, [SPIKE, orphan])
    assert any("stream 'burst' is stopped both by drain and by abandon" in e for e in errs)
