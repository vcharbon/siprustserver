"""A crash kills the process (lib/crash.sh): every container's task gets
SIGKILL by container id and is proven dead by its runtime state before inject
returns, and the probe proves the crashed incarnation and its successor never
ran together. `force_delete` is the other shape: the API forgets the pod while
its process runs on, and the probe follows the elder by container id and
measures the overlap."""

from __future__ import annotations

import json
from pathlib import Path

from exe_kit import Rule, Sandbox, crash_rules, facts, failed, live

T0 = 1_792_103_350_000
STATE = r"docker exec n1 crictl inspect --output go-template --template \{\{\.status\.state\}\} c1$"


def iso_ms(ms: int) -> str:
    from datetime import UTC, datetime

    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}000000Z"


LIMITER = live("app=call-limiter", "l-0 u0 n1 True\n")


def test_a_crash_kills_every_container_by_id_and_proves_it_dead_before_returning(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_kill")
    sb.rules(LIMITER, *crash_rules("l-0", "n1", [("limiter", "c1")]))
    f = facts(sb.run("inject"))
    kill, proof = sb.index("task kill -s SIGKILL c1"), [i for i, c in enumerate(sb.calls()) if "{{.status.state}} c1" in c.line]
    assert 0 <= kill < proof[-1] and proof[0] < kill  # running before, dead after
    assert sb.index(" delete pod ") == -1 and sb.index(" kill -9 ") == -1
    assert f["containers"] == ["l-0/limiter"]
    assert isinstance(f["dead_at_ms"], int) and isinstance(f["killed_at_ms"], int) and f["dead_at_ms"] >= f["killed_at_ms"]
    assert (sb.event_dir / "crashed").read_text() == "l-0 u0 n1 limiter c1 0\n"


def test_a_container_the_runtime_no_longer_knows_is_dead(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_kill")
    sb.rules(LIMITER, *crash_rules("l-0", "n1", [("limiter", "c1")], after="x"))
    rules = json.loads(sb.rules_file.read_text())
    gone = {"match": STATE, "stdout": "rpc error: code = NotFound desc = not found", "rc": 1, "once": False}
    sb.rules_file.write_text(json.dumps([r for r in rules if not (r["match"] == STATE and not r["once"])] + [gone]))
    facts(sb.run("inject"))


def test_a_state_that_cannot_be_read_is_no_proof_of_death(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_kill")
    sb.rules(LIMITER, *crash_rules("l-0", "n1", [("limiter", "c1")]))
    rules = json.loads(sb.rules_file.read_text())
    broken = {"match": STATE, "stdout": "Error response from daemon: container is paused", "rc": 1, "once": False}
    sb.rules_file.write_text(json.dumps([r for r in rules if not (r["match"] == STATE and not r["once"])] + [broken]))
    failed(sb.run("inject"), "l-0/limiter is unreadable 1s after SIGKILL")


def test_a_container_that_survives_its_sigkill_fails_the_inject(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker")
    sb.rules(live("app=b2bua-worker", "w-0 u0 n1 True\n"),
             *crash_rules("w-0", "n1", [("worker", "c1")], after="CONTAINER_RUNNING"))
    failed(sb.run("inject"), "w-0/worker is running 1s after SIGKILL")


def crashed_probe(tmp_path: Path, restarts: int, elder_last: int, successor_first: int) -> Sandbox:
    sb = Sandbox(tmp_path, "pod_kill")
    sb.state("targets", "l-0 u0 n1\n")
    sb.state("live_before", "1\n")
    sb.state("crashed", "l-0 u0 n1 limiter c1 0\n")
    pod = {
        "metadata": {"uid": "u0"},
        "status": {
            "conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": "2026-10-15T22:29:20Z"}],
            "containerStatuses": [
                {"name": "limiter", "restartCount": restarts, "state": {"running": {"startedAt": "2026-10-15T22:29:12Z"}}}
            ],
        },
    }
    sb.rules(
        Rule(r"get pod l-0 --ignore-not-found -o json$", json.dumps(pod)),
        Rule(r"logs l-0 -c limiter --previous --timestamps --tail=1$", f"{iso_ms(elder_last)} last words\n"),
        Rule(r"logs l-0 -c limiter --timestamps --limit-bytes=8192$", f"{iso_ms(successor_first)} first words\n{iso_ms(successor_first + 5)} more\n"),
    )
    return sb


def test_the_crash_probe_waits_until_the_container_restarted(tmp_path: Path) -> None:
    assert crashed_probe(tmp_path, 0, T0, T0 + 900).run("probe").returncode == 75


def test_the_crash_probe_reports_the_restart_with_no_overlap(tmp_path: Path) -> None:
    f = facts(crashed_probe(tmp_path, 1, T0, T0 + 900).run("probe"))
    (c,) = f["containers"]  # type: ignore[misc]
    assert c == {
        "pod": "l-0", "container": "limiter", "replaced": False,
        "restarted_at_ms": 1792103352000, "ready_at_ms": 1792103360000,
        "elder_last_log_ms": T0, "successor_first_log_ms": T0 + 900, "overlap_ms": 0,
    }


def test_two_incarnations_running_together_fail_the_crash_probe(tmp_path: Path) -> None:
    failed(crashed_probe(tmp_path, 1, T0 + 1500, T0 + 900).run("probe"), "logged 600 ms after its successor started")


def test_the_crash_probe_waits_while_the_pod_cannot_be_read(tmp_path: Path) -> None:
    sb = crashed_probe(tmp_path, 1, T0, T0 + 900)
    sb.rules(Rule(r"get pod l-0 --ignore-not-found -o json$", rc=1))
    assert sb.run("probe").returncode == 75


# ── force_delete ──────────────────────────────────────────────────────────


def test_force_delete_leaves_the_process_and_deletes_the_pod(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "force_delete")
    sb.rules(live("app=b2bua-worker", "w-0 u0 n1 True\n"), *crash_rules("w-0", "n1", [("worker", "c1")]),
             Rule(r"delete pod w-0 --grace-period=0 --force --wait=false$"))
    facts(sb.run("inject"))
    assert sb.index("delete pod w-0") >= 0 and sb.index("task kill") == -1


def force_delete_probe(tmp_path: Path, elder: Rule) -> Sandbox:
    sb = Sandbox(tmp_path, "force_delete")
    sb.state("targets", "w-0 u0 n1\n")
    sb.state("live_before", "2\n")
    sb.state("crashed", "w-0 u0 n1 worker c1 0\n")
    t = "2026-10-15T22:29:20Z"
    items = [{"metadata": {"name": n, "uid": u}, "status": {
        "conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": t}],
        "containerStatuses": [{"state": {"running": {"startedAt": t}}}]}} for n, u in (("w-0", "u9"), ("w-1", "u1"))]
    successor = {"spec": {"nodeName": "n2"}, "status": {"containerStatuses": [{"name": "worker", "containerID": "containerd://c9"}]}}
    sb.rules(
        elder,
        Rule(r"docker exec n1 crictl inspect --output go-template --template \{\{\.status\.finishedAt\}\} c1$", iso_ms(T0 + 2200)),
        Rule(r"get pods -l app=b2bua-worker -o json$", json.dumps({"items": items})),
        Rule(r"get pod w-0 --ignore-not-found -o json$", json.dumps(successor)),
        Rule(r"docker exec n2 crictl inspect --output go-template --template \{\{\.status\.startedAt\}\} c9$", iso_ms(T0 + 300)),
    )
    return sb


def test_the_force_delete_probe_follows_the_elder_until_it_exits_and_measures_the_overlap(tmp_path: Path) -> None:
    assert force_delete_probe(tmp_path, Rule(STATE, "CONTAINER_RUNNING")).run("probe").returncode == 75
    f = facts(force_delete_probe(tmp_path, Rule(STATE, "CONTAINER_EXITED")).run("probe"))
    assert f["elders"] == [{
        "container": "w-0/worker", "elder_end_ms": T0 + 2200, "elder_end_seen": "exited",
        "successor_started_at_ms": T0 + 300, "overlap_ms": 1900,
    }]


def test_the_force_delete_probe_takes_an_elder_the_runtime_forgot_as_ended_when_first_seen_gone(tmp_path: Path) -> None:
    sb = force_delete_probe(tmp_path, Rule(STATE, "rpc error: code = NotFound", rc=1))
    f = facts(sb.run("probe"))
    (e,) = f["elders"]  # type: ignore[misc]
    assert e["elder_end_seen"] == "gone" and e["elder_end_ms"] > T0  # type: ignore[index,operator]


def test_the_force_delete_probe_waits_while_the_elder_cannot_be_read(tmp_path: Path) -> None:
    sb = force_delete_probe(tmp_path, Rule(STATE, "Cannot connect to the Docker daemon", rc=1))
    assert sb.run("probe").returncode == 75
    assert not (sb.event_dir / "elders").exists() or json.loads((sb.event_dir / "elders").read_text()) == {}
