"""The Chaos Mesh incidents (lib/chaos-mesh.sh) against fake `kubectl`: a
resource per event under a hashed name, created (never applied over an
existing one), injected before inject returns, and proven gone at heal."""

from __future__ import annotations

import hashlib
import json
from pathlib import Path

from exe_kit import Rule, Sandbox, facts, failed, live

from hakit import jsonval as jv
from hakit.jsonval import Json, as_obj

NAME = "hakit-" + hashlib.sha1(b"r-1/001-partition/partition").hexdigest()[:10] + "-partition"


def created(sb: Sandbox) -> Json:
    (doc,) = [as_obj(json.loads(c.stdin)) for c in sb.calls() if c.argv[-3:] == ["create", "-f", "-"]]
    return doc


def partition(tmp_path: Path, workers: str, **params: str) -> Sandbox:
    sb = Sandbox(tmp_path, "partition", **params)
    records = {"status": {"experiment": {"containerRecords": [{"id": "ns/w-0", "selectorKey": "."}]}}}
    sb.rules(
        Rule(r"get pods -l app=[a-z0-9-]+ -o json$", '{"items": [{"spec": {}}]}'),
        live("app=b2bua-worker", workers),
        live("app=other", "o-0 u5 n5 True\n"),
        Rule(r"create -f -$"),
        Rule(rf"get networkchaos {NAME} -o jsonpath", "", once=True),
        Rule(rf"get networkchaos {NAME} -o jsonpath", "True"),
        Rule(rf"get networkchaos {NAME} -o json$", json.dumps(records)),
    )
    sb.env["HAKIT_TARGET_PICK"] = "one"
    return sb


def test_partition_cuts_the_target_from_the_other_pods_never_from_itself(tmp_path: Path) -> None:
    sb = partition(tmp_path, "w-0 u0 n1 True\n")
    res = sb.run("inject")
    failed(res, "no live pod of app=b2bua-worker is left to be the targets' peer")
    sb = partition(tmp_path, "w-0 u0 n1 True\nw-1 u1 n2 True\nw-2 u2 n3 True\n")
    f = facts(sb.run("inject"))
    doc = created(sb)
    assert doc["metadata"] == {"name": NAME, "namespace": "ns", "labels": {"hakit.io/run": "r-1"}}
    spec = as_obj(doc["spec"])
    (target,) = jv.strings(jv.obj(jv.obj(spec, "selector"), "pods"), "ns")
    peers = jv.strings(jv.obj(jv.obj(jv.obj(spec, "target"), "selector"), "pods"), "ns")
    assert target not in peers and sorted([target, *peers]) == ["w-0", "w-1", "w-2"]
    assert (spec["action"], spec["direction"], spec["mode"]) == ("partition", "both", "all")
    assert sorted(jv.strings(f, "peers")) == sorted(peers)


def test_partition_with_a_peer_selector_cuts_the_target_from_those_pods(tmp_path: Path) -> None:
    sb = partition(tmp_path, "w-0 u0 n1 True\n", peer_selector="app=other")
    facts(sb.run("inject"))
    spec = as_obj(created(sb)["spec"])
    assert spec["target"] == {"mode": "all", "selector": {"namespaces": ["ns"], "labelSelectors": {"app": "other"}}}  # type: ignore[index]


def test_partition_refuses_host_network_pods_before_creating_anything(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "partition")
    sb.rules(Rule(r"get pods -l app=b2bua-worker -o json$", '{"items": [{"spec": {"hostNetwork": true}}]}'))
    failed(sb.run("inject"), "selects host-network pods")
    assert sb.index(" create ") == -1


def test_an_existing_resource_name_is_refused(tmp_path: Path) -> None:
    sb = partition(tmp_path, "w-0 u0 n1 True\nw-1 u1 n2 True\n")
    rules = json.loads(sb.rules_file.read_text())
    sb.rules_file.write_text(json.dumps([{"match": r"create -f -$", "stdout": "", "rc": 1, "once": False}, *rules]))
    res = sb.run("inject")
    assert res.returncode == 1 and "unscripted" not in res.stderr and sb.index(f"get networkchaos {NAME}") == -1


def clock_heal(tmp_path: Path, rules: list[Rule]) -> Sandbox:
    sb = Sandbox(tmp_path, "clock_skew")
    sb.rules(*rules)
    return sb


def test_a_chaos_mesh_heal_proves_the_resource_gone_twice(tmp_path: Path) -> None:
    sb = clock_heal(tmp_path, [Rule(r"delete timechaos hakit-[0-9a-f]{10}-clock-skew --ignore-not-found --wait=true"),
                               Rule(r"get timechaos hakit-[0-9a-f]{10}-clock-skew --ignore-not-found -o name$", "")])
    facts(sb.run("heal"))
    assert len(sb.lines("--ignore-not-found -o name")) == 2


def test_a_chaos_mesh_heal_fails_when_the_resource_is_back_a_second_later(tmp_path: Path) -> None:
    sb = clock_heal(tmp_path, [Rule(r"delete timechaos"),
                               Rule(r"get timechaos .* --ignore-not-found -o name$", "", once=True),
                               Rule(r"get timechaos .* --ignore-not-found -o name$", "timechaos/x")])
    failed(sb.run("heal"), "exists again a second after its deletion")


def test_a_chaos_mesh_heal_drops_the_finalizer_only_of_a_resource_that_injected_nothing(tmp_path: Path) -> None:
    nothing = {"status": {"experiment": {"containerRecords": [{"phase": "Not Injected"}]}}}
    sb = clock_heal(tmp_path, [Rule(r"delete timechaos", rc=1), Rule(r"get timechaos .* -o json$", json.dumps(nothing)),
                               Rule(r"patch timechaos .* --type=merge"),
                               Rule(r"get timechaos .* --ignore-not-found -o name$", "")])
    facts(sb.run("heal"))
    assert sb.index("patch timechaos") >= 0
    injected = {"status": {"experiment": {"containerRecords": [{"phase": "Injected"}]}}}
    sb.rules(Rule(r"delete timechaos", rc=1), Rule(r"get timechaos .* -o json$", json.dumps(injected)))
    failed(sb.run("heal"), "not recovered within 90s")
    assert sb.index(" patch ") == -1


def test_a_chaos_mesh_heal_fails_when_the_api_cannot_answer(tmp_path: Path) -> None:
    sb = clock_heal(tmp_path, [Rule(r"delete timechaos"), Rule(r"get timechaos .* --ignore-not-found -o name$", rc=1)])
    res = sb.run("heal")
    assert res.returncode == 1 and "unscripted" not in res.stderr and res.stdout == ""


def test_io_stall_delays_the_volume_it_names(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "io_stall", delay_ms="250", container="rabbitmq")
    sb.rules(Rule(r"create -f -$"), Rule(r"get iochaos .* -o jsonpath", "True"), Rule(r"get iochaos .* -o json$", "{}"))
    facts(sb.run("inject"))
    doc = created(sb)
    assert doc["kind"] == "IOChaos"
    spec = as_obj(doc["spec"])
    assert {k: spec[k] for k in ("action", "volumePath", "path", "delay", "percent", "containerNames")} == {
        "action": "latency", "volumePath": "/var/lib/rabbitmq", "path": "/var/lib/rabbitmq/**",
        "delay": "250ms", "percent": 100, "containerNames": ["rabbitmq"]}
