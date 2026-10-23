"""The `loadgen` stream driver (ADR-0049 §2.8, §4.5, §5.4): a product image's
loadgen binary runs a stream at its slot address with the runner's flags, is
drained by SIGUSR1 and must then exit 0, is reconciled against the counters
of its last `/metrics` body, and a profile whose image refuses its flags or
does not register its shape is refused before the run."""

from __future__ import annotations

import json
from pathlib import Path

import pytest
from kit import budget, write_yaml

from hakit.check import profile_errors
from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, ProcResult, Rule
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.loadgen import parse_metrics
from hakit.paths import BASE_CONTENT
from hakit.profile import parse_profile
from hakit.runner import Runner, RunRefused
from hakit.schemas import SchemaSet
from hakit.jsonl import read_jsonl

SCHEMAS = SchemaSet.load()
IMAGE = "product:dev"
SOFT = ["docker", "kill", "--signal", "USR1", "hakit-burst"]
HARD = ["docker", "kill", "hakit-burst"]
SPIKE: Json = {"at_s": 30, "incident": "spike", "stream": "burst", "budget": "any"}


def base() -> Json:
    return {
        "name": "basic",
        "driver": "sipp",
        "shape": "basic",
        "scenario": "uac-basic-hold.xml",
        "cps": 5,
        "setup_deadline_s": 10,
        "ring_s": 5,
        "hold_s": 20,
    }


def burst(**params: object) -> Json:
    return {
        "name": "burst",
        "driver": "loadgen",
        "start": "incident",
        "shape": "basic",
        "cps": 50,
        "setup_deadline_s": 10,
        "ring_s": 1,
        "hold_s": 2,
        "params": {"image": IMAGE, "command": "/bin/product", **params},
    }


def content(tmp_path: Path, events: list[Json], streams: list[Json]) -> Content:
    root = tmp_path / "entry"
    budget(root, "any")
    doc: Json = {
        "schema": "profile.v1",
        "name": "load",
        "description": "d",
        "duration_s": 600,
        "drain_s": 130,
        "settle_s": 15,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": streams,
        "schedule": {"at": events},
    }
    write_yaml(root / "profiles" / "load.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def run(
    tmp_path: Path, rules: list[Rule], events: list[Json] | None = None, streams: list[Json] | None = None
) -> tuple[Path, list[Call]]:
    clock, fake = simulated(rules, shapes=["basic"])
    c = content(tmp_path, events or [SPIKE], streams or [base(), burst()])
    r = Runner(c, SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    return r.run("load"), fake.calls


def metrics(offered: int, not_placed: int = 0, failed: int = 0, stream: str = "burst") -> str:
    return (
        f'loadgen_calls_offered_total{{stream="{stream}",shape="basic"}} {offered}\n'
        f'loadgen_not_placed_total{{stream="{stream}",shape="basic",reason="shed"}} {not_placed}\n'
        f'loadgen_generator_failures_total{{stream="{stream}",shape="basic",reason="panic"}} {failed}\n'
    )


def final_metrics(body: str) -> Rule:
    def rule(c: Call) -> str | None:
        if c.kind == "read" and c.path is not None and c.path.name == "metrics.prom" and "burst" in str(c.path):
            return body
        return None

    return rule


def started(calls: list[Call]) -> list[list[str]]:
    return [
        c.argv for c in calls if c.kind == "run" and c.argv[:2] == ["docker", "run"] and "hakit-burst" in c.argv
    ]


def phase_at(run_dir: Path, event: str) -> dict[str, int]:
    return {
        str(e["phase"]): int(str(e["at_ms"]))
        for e in read_jsonl(run_dir / "events.jsonl")
        if e.get("kind") == "phase" and e.get("event") == event
    }


def verdict(run_dir: Path) -> Json:
    return as_obj(json.loads((run_dir / "verdict.json").read_text()))


def flag(argv: list[str], name: str) -> str:
    """The value of `name`, given as `name value` or `name=value`."""
    for i, a in enumerate(argv):
        if a == name:
            return argv[i + 1]
        if a.startswith(f"{name}="):
            return a.split("=", 1)[1]
    raise AssertionError(f"{name} not in {argv}")


def test_a_loadgen_firing_runs_the_product_image_at_its_slot_with_the_runners_flags(tmp_path: Path) -> None:
    streams = [base(), burst(case="/cases/c.json", auto_retransmit=True, quiet=False)]
    run_dir, calls = run(tmp_path, [final_metrics(metrics(0))], streams=streams)
    [argv] = started(calls)
    lab = lab_from_env({})
    assert flag(argv, "--network") == lab.sipp_network and flag(argv, "--ip") == lab.sipp_ip(1)
    image_at = argv.index(IMAGE)
    assert argv[image_at + 1] == "/bin/product"
    # The working directory on the generator scratch (workdir.py), never the run directory.
    assert flag(argv, "-v") == f"{lab.generator_scratch}/{run_dir.name}/streams/burst/001-spike:/stats"
    assert flag(argv, "--target") == lab.sip_target and flag(argv, "--bind-ip") == lab.sipp_ip(1)
    assert flag(argv, "--scenario") == "basic" and flag(argv, "--cps") == "50"
    assert (flag(argv, "--run"), flag(argv, "--stream")) == (run_dir.name, "burst")
    assert "--call-id-tag=-001-spike" in argv  # `=`: the tag starts with `-`
    assert flag(argv, "--records-file") == "/stats/records.jsonl"
    assert flag(argv, "--records-url") == lab.vlogs_url_from_generators
    assert (flag(argv, "--ring-delay-ms"), flag(argv, "--talk-time-ms")) == ("1000", "2000")
    assert flag(argv, "--recv-timeout-ms") == "10000"
    assert flag(argv, "--case") == "/cases/c.json" and "--auto-retransmit" in argv and "--quiet" not in argv


def test_the_call_limit_is_the_in_flight_cap(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, [final_metrics(metrics(0))])
    [argv] = started(calls)
    worst = 10 + 1 + 2 + 32
    assert flag(argv, "--max-in-flight") == str(50 * worst + 10)
    docs = {str(d["stream"]): d for d in json.loads((run_dir / "streams.json").read_text())}
    assert docs["burst"]["call_limit"] == 50 * worst + 10

    capped = burst()
    capped["max_concurrent"] = 77
    _, calls = run(tmp_path / "capped", [final_metrics(metrics(0))], streams=[base(), capped])
    assert flag(started(calls)[0], "--max-in-flight") == "77"


def test_a_drained_firing_exits_0_and_heals_at_its_exit(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, [final_metrics(metrics(0))])
    at = phase_at(run_dir, "001-spike")
    assert "healed" in at and "failed" not in at
    argvs = [c.argv for c in calls if c.kind == "run"]
    assert SOFT in argvs and HARD not in argvs
    assert verdict(run_dir)["status"] == "pass"


def test_a_drained_firing_that_exits_non_zero_fails_its_heal(tmp_path: Path) -> None:
    def exits_3(c: Call) -> ProcResult | None:
        if c.kind == "run" and c.argv[:2] == ["docker", "inspect"] and "ExitCode" in c.argv[3] and c.argv[-1] == "hakit-burst":
            return ProcResult(0, "3\n", "")
        return None

    run_dir, _ = run(tmp_path, [exits_3, final_metrics(metrics(0))])
    at = phase_at(run_dir, "001-spike")
    assert "failed" in at and "healed" not in at
    assert verdict(run_dir)["status"] == "invalid"


def test_the_records_reconcile_with_the_calls_not_placed_and_failed(tmp_path: Path) -> None:
    run_dir, _ = run(tmp_path, [final_metrics(metrics(3, not_placed=2, failed=1))])
    docs = {str(d["stream"]): d for d in json.loads((run_dir / "streams.json").read_text())}
    assert (docs["burst"]["calls"], docs["burst"]["not_placed"], docs["burst"]["generator_failures"]) == (3, 2, 1)
    kinds = [str(as_obj(i)["kind"]) for i in verdict(run_dir)["integrity"]]  # type: ignore[union-attr]
    assert kinds == ["generator_failure"]  # 0 records + 2 not placed + 1 failed = 3: no gap

    run_dir, _ = run(tmp_path / "gap", [final_metrics(metrics(5, not_placed=2))])
    details = [str(as_obj(i)["detail"]) for i in verdict(run_dir)["integrity"]]  # type: ignore[union-attr]
    assert details == ["0 records + 2 not placed + 0 failed by the generator != 5 calls the generator counted"]


def test_an_unreadable_counter_is_a_record_gap(tmp_path: Path) -> None:
    run_dir, _ = run(tmp_path, [final_metrics("")])
    kinds = [str(as_obj(i)["kind"]) for i in verdict(run_dir)["integrity"]]  # type: ignore[union-attr]
    assert kinds == ["record_gap"]


def test_an_abandoned_loadgen_firing_is_killed_and_not_reconciled(tmp_path: Path) -> None:
    orphan: Json = {"at_s": 30, "incident": "orphan", "stream": "burst", "budget": "any"}
    run_dir, calls = run(tmp_path, [final_metrics(metrics(9))], events=[orphan])
    argvs = [c.argv for c in calls if c.kind == "run"]
    assert HARD in argvs and SOFT not in argvs
    docs = {str(d["stream"]): d for d in json.loads((run_dir / "streams.json").read_text())}
    assert docs["burst"]["reconciled"] is False and docs["burst"]["calls"] == 9
    assert verdict(run_dir)["status"] == "pass"


def test_two_firings_carry_their_own_call_id_tags(tmp_path: Path) -> None:
    second: Json = {**SPIKE, "at_s": 300}
    _, calls = run(tmp_path, [final_metrics(metrics(0))], events=[SPIKE, second])
    assert [flag(a, "--call-id-tag") for a in started(calls)] == ["-001-spike", "-002-spike"]


def test_an_image_that_refuses_the_flags_refuses_the_profile(tmp_path: Path) -> None:
    def refuses(c: Call) -> ProcResult | None:
        if c.kind == "run" and "--list-shapes" in c.argv:
            return ProcResult(2, "", "error: unexpected argument '--frobnicate'")
        return None

    with pytest.raises(RunRefused, match="refused its flags: error: unexpected argument"):
        run(tmp_path, [refuses], streams=[base(), burst(frobnicate=1)])


def test_an_image_that_does_not_register_the_shape_refuses_the_profile(tmp_path: Path) -> None:
    def other(c: Call) -> ProcResult | None:
        if c.kind == "run" and "--list-shapes" in c.argv:
            return ProcResult(0, "other_shape\n", "")
        return None

    with pytest.raises(RunRefused, match="shape 'basic' is not registered in its loadgen image"):
        run(tmp_path, [other])


def test_a_loadgen_stream_names_its_product_and_no_flag_the_runner_sets(tmp_path: Path) -> None:
    bad = burst(cps=3)
    del as_obj(bad["params"])["image"]
    c = content(tmp_path, [SPIKE], [base(), bad])
    errs = profile_errors(parse_profile(as_obj(c.profiles["load"].raw)), c)
    assert any("params.image must name the product's image" in e for e in errs)
    assert any("params.cps is a flag the runner sets itself" in e for e in errs)


def test_parse_metrics_sums_the_streams_series_over_their_labels() -> None:
    body = metrics(4, not_placed=1) + metrics(9, stream="other") + (
        'loadgen_calls_offered_total{stream="burst",shape="second"} 6\n# HELP x\n'
    )
    t = parse_metrics(body, "burst")
    assert (t.calls, t.not_placed, t.generator_failures) == (10, 1, 0)
    assert parse_metrics("", "burst").calls is None
