"""A loadgen stream placed in the cluster (`placement: pod`): one pod with the
binary and a keeper sharing `/stats`, its target a Service resolved to its
cluster IP, drained and stepped through the API server's pod proxy, its files
copied out before the pod goes; and the rate steps of a profile applied to
the running generators and logged in `steps.jsonl`."""

from __future__ import annotations

import json
from pathlib import Path

from kit import budget, write_yaml

from hakit.check import profile_errors
from hakit.content import Content, discover
from hakit.dryrun import simulated
from hakit.executor import Call, ProcResult, Rule
from hakit.jsonl import read_jsonl
from hakit.jsonval import Json, as_obj
from hakit.lab import lab_from_env
from hakit.paths import BASE_CONTENT
from hakit.pod_loadgen import pod_name
from hakit.profile import parse_profile
from hakit.runner import Runner
from hakit.schemas import SchemaSet

SCHEMAS = SchemaSet.load()


def cell(name: str, **over: object) -> Json:
    return {
        "name": name,
        "driver": "loadgen",
        "placement": "pod",
        "target": "cell-0:5060",
        "shape": "basic",
        "cps": 4,
        "setup_deadline_s": 10,
        "ring_s": 1,
        "hold_s": 2,
        "params": {"image": "product:dev", "command": "/bin/product"},
        **over,
    }


def content(tmp_path: Path, streams: list[Json]) -> Content:
    root = tmp_path / "entry"
    budget(root, "any")
    doc: Json = {
        "schema": "profile.v1",
        "name": "pods",
        "description": "d",
        "duration_s": 300,
        "drain_s": 60,
        "settle_s": 15,
        "setup_slow_ms": 1000,
        "catalogs": ["x-api-call"],
        "streams": streams,
    }
    write_yaml(root / "profiles" / "pods.yaml", doc)
    return discover([BASE_CONTENT, root], SCHEMAS)


def run(tmp_path: Path, streams: list[Json], rules: list[Rule] | None = None) -> tuple[Path, list[Call]]:
    clock, fake = simulated(rules or [], shapes=["basic"])
    r = Runner(content(tmp_path, streams), SCHEMAS, lab_from_env({}), fake, clock, tmp_path / "results", say=lambda _m: None)
    return r.run("pods"), fake.calls


def kubectl(calls: list[Call], verb: str) -> list[list[str]]:
    return [c.argv for c in calls if c.kind == "run" and c.argv[:1] == ["kubectl"] and verb in c.argv]


def test_a_pod_stream_runs_the_binary_at_the_pods_address_against_the_resolved_service(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, [cell("cell_a")])
    assert ["get", "service", "cell-0", "-o", "jsonpath={.spec.clusterIP}"] == kubectl(calls, "service")[0][5:]
    pod = as_obj(json.loads((run_dir / "streams" / "cell_a" / "pod.json").read_text()))
    meta, spec = as_obj(pod["metadata"]), as_obj(pod["spec"])
    assert meta["name"] == "hakit-cell-a" == pod_name("cell_a", "")
    assert as_obj(meta["labels"]) == {"hakit-run": run_dir.name, "hakit-stream": "cell_a"}
    assert as_obj(meta["annotations"])["prometheus.io/scrape"] == "true"
    assert spec["nodeSelector"] == {"tier": "edge"}
    gen, keeper = (as_obj(c) for c in spec["containers"])  # type: ignore[union-attr]
    args = [str(a) for a in gen["args"]]  # type: ignore[union-attr]
    assert args[args.index("--target") + 1] == "10.96.0.10:5060"
    assert args[args.index("--bind-ip") + 1] == "$(POD_IP)"
    assert args[args.index("--records-url") + 1] == "http://172.20.0.1:9428"
    # The in-cluster scrape keeps the declared port its annotation names.
    assert gen["ports"] == [{"name": "metrics", "containerPort": 9300}]
    assert as_obj(meta["annotations"])["prometheus.io/port"] == "9300"
    assert keeper["name"] == "keeper"
    # The pod is ready before the load starts; the run starts no docker generator.
    assert any("--for=condition=Ready" in a for a in kubectl(calls, "wait"))
    assert not [c for c in calls if c.argv[:2] == ["docker", "run"] and "--list-shapes" not in c.argv]


def test_a_pod_stream_drains_through_the_pod_proxy_and_its_files_are_copied_out(tmp_path: Path) -> None:
    run_dir, calls = run(tmp_path, [cell("cell_a")])
    raws = [a[a.index("--raw") + 1] for a in kubectl(calls, "--raw")]
    assert raws == ["/api/v1/namespaces/sip-test/pods/hakit-cell-a:9300/proxy/drain"]
    (doc,) = json.loads((run_dir / "streams.json").read_text())
    assert doc["drained"] is True and doc["calls"] == 0
    assert (run_dir / "streams" / "cell_a" / "metrics.prom").is_file()
    deleted = kubectl(calls, "delete")
    assert deleted[-1][5:8] == ["delete", "pod", "hakit-cell-a"]


def test_a_pod_that_never_gets_ready_refuses_the_run(tmp_path: Path) -> None:
    def never_ready(c: Call) -> ProcResult | None:
        return ProcResult(1, "", "timed out") if c.argv[:1] == ["kubectl"] and "wait" in c.argv else None

    try:
        run(tmp_path, [cell("cell_a")], [never_ready])
    except Exception as e:  # RunRefused
        assert "not ready" in str(e)
    else:
        raise AssertionError("the run started without its generator")


def test_only_a_loadgen_stream_runs_as_a_pod(tmp_path: Path) -> None:
    sipp: Json = {
        "name": "sipp_pod",
        "driver": "sipp",
        "placement": "pod",
        "shape": "basic",
        "scenario": "uac-basic-hold.xml",
        "cps": 4,
        "setup_deadline_s": 10,
        "ring_s": 1,
        "hold_s": 2,
    }
    c = content(tmp_path, [cell("cell_a")])
    p = parse_profile({**c.profiles["pods"].raw, "streams": [sipp]})
    assert any("only a loadgen stream runs as a pod" in e for e in profile_errors(p, c))


def test_rate_steps_are_applied_to_the_running_generators_and_logged(tmp_path: Path) -> None:
    stepped = cell("cell_a", steps=[{"at_s": 60, "cps": 8}, {"at_s": 120, "cps": 12}])
    docker: Json = {**cell("lg_docker"), "placement": "docker", "steps": [{"at_s": 90, "cps": 6}]}
    del docker["target"]
    run_dir, calls = run(tmp_path, [stepped, docker])
    raws = [a[a.index("--raw") + 1] for a in kubectl(calls, "--raw")]
    assert "/api/v1/namespaces/sip-test/pods/hakit-cell-a:9300/proxy/rate?cps=8" in raws
    assert "/api/v1/namespaces/sip-test/pods/hakit-cell-a:9300/proxy/rate?cps=12" in raws
    posts = [c.url for c in calls if c.kind == "http" and c.method == "POST" and "/rate" in c.url]
    assert posts == ["http://192.168.60.101:9300/rate?cps=6"]
    steps = read_jsonl(run_dir / "steps.jsonl")
    assert [(s["stream"], s["at_s"], s["cps"]) for s in steps] == [("cell_a", 60, 8), ("lg_docker", 90, 6), ("cell_a", 120, 12)]
    assert all(s["applied_ms"] == s["due_ms"] for s in steps)


def test_a_refused_rate_step_is_a_generator_failure(tmp_path: Path) -> None:
    def refuse(c: Call) -> ProcResult | None:
        return ProcResult(1, "", "404") if c.argv[:1] == ["kubectl"] and "--raw" in c.argv and "rate" in c.argv[-3] else None

    run_dir, _ = run(tmp_path, [cell("cell_a", steps=[{"at_s": 60, "cps": 8}])], [refuse])
    found = read_jsonl(run_dir / "integrity.jsonl")
    assert [(i["kind"], i["stream"]) for i in found] == [("generator_failure", "cell_a")]
    assert json.loads((run_dir / "verdict.json").read_text())["status"] == "invalid"


def test_a_finding_is_in_integrity_jsonl_while_the_run_still_runs(tmp_path: Path) -> None:
    seen_at_drain: list[str] = []

    def refuse_then_look(c: Call) -> ProcResult | None:
        if c.argv[:1] != ["kubectl"] or "--raw" not in c.argv:
            return None
        if "rate" in c.argv[-3]:
            return ProcResult(1, "", "404")
        found = list((tmp_path / "results").glob("*/integrity.jsonl"))
        seen_at_drain.extend(p.read_text() for p in found)
        seen_at_drain.extend(p.read_text() for p in (tmp_path / "results").glob("*/run-meta.json"))
        return None

    run(tmp_path, [cell("cell_a", steps=[{"at_s": 60, "cps": 8}])], [refuse_then_look])
    integrity, meta = seen_at_drain
    assert '"generator_failure"' in integrity
    # The load's start is in run-meta.json from the moment it starts.
    assert "load_started_at_ms" in meta


def test_an_unread_pod_state_is_no_dead_generator(tmp_path: Path) -> None:
    # The API server does not answer three reads: the generator is not
    # declared dead; a pod that is gone is.
    misses = [3]

    def flaky(c: Call) -> ProcResult | None:
        if c.argv[:1] == ["kubectl"] and "get" in c.argv and "pod" in c.argv and "json" in c.argv and misses[0] > 0:
            misses[0] -= 1
            return ProcResult(1, "", "Unable to connect to the server: connection refused")
        return None

    run_dir, _ = run(tmp_path / "flaky", [cell("cell_a")], [flaky])
    assert read_jsonl(run_dir / "integrity.jsonl") == []

    def gone(c: Call) -> ProcResult | None:
        if c.argv[:1] == ["kubectl"] and "get" in c.argv and "pod" in c.argv and "json" in c.argv:
            return ProcResult(1, "", 'Error from server (NotFound): pods "hakit-cell-a" not found')
        return None

    run_dir, _ = run(tmp_path / "gone", [cell("cell_a")], [gone])
    assert [i["kind"] for i in read_jsonl(run_dir / "integrity.jsonl")][:1] == ["stream_died"]


def test_a_load_incident_stream_takes_no_steps(tmp_path: Path) -> None:
    c = content(tmp_path, [cell("cell_a")])
    p = parse_profile({**c.profiles["pods"].raw, "streams": [cell("cell_a", start="incident", steps=[{"at_s": 1, "cps": 2}])]})
    assert any("has no rate steps" in e for e in profile_errors(p, c))
