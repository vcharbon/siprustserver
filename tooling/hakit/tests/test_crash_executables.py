"""A crash kills the process (lib/crash.sh): every container's process gets
SIGKILL from its node and is proven dead before inject returns, and the probe
proves the crashed incarnation and its successor never ran together.
`force_delete` is the other shape: the API forgets the pod while its process
runs on, and the probe measures the overlap."""

from __future__ import annotations

import json
from pathlib import Path

from exe_kit import Rule, Sandbox, crash_rules, facts

T0 = 1_792_103_350_000


def iso_ms(ms: int) -> str:
    from datetime import UTC, datetime

    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%dT%H:%M:%S.") + f"{ms % 1000:03d}000000Z"


def test_a_crash_kills_every_process_and_proves_it_dead_before_returning(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_kill")
    sb.rules(
        Rule(r"get pods -l app=call-limiter -o jsonpath=.*spec\.nodeName", "l-0 u0 n1 True\n"),
        *crash_rules("l-0", "n1", [("limiter", "c1", 501)]),
    )
    f = facts(sb.run("inject"))
    assert 0 <= sb.index("docker exec n1 kill -9 501") < sb.index("docker exec n1 test -d /proc/501")
    assert sb.index(" delete pod ") == -1
    assert f["containers"] == ["l-0/limiter"]
    assert isinstance(f["dead_at_ms"], int) and isinstance(f["killed_at_ms"], int) and f["dead_at_ms"] >= f["killed_at_ms"]
    assert (sb.event_dir / "crashed").read_text() == "l-0 u0 n1 limiter c1 501 0\n"


def test_a_process_that_survives_its_sigkill_fails_the_inject(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker")
    sb.env["HK_CRASH_WAIT_S"] = "1"
    sb.rules(
        Rule(r"get pods -l app=b2bua-worker -o jsonpath=.*spec\.nodeName", "w-0 u0 n1 True\n"),
        *crash_rules("w-0", "n1", [("worker", "c1", 77)], alive=True),
    )
    res = sb.run("inject")
    assert res.returncode == 1 and "is still alive" in res.stderr


def crashed_probe(tmp_path: Path, restarts: int, elder_last: int, successor_first: int) -> Sandbox:
    sb = Sandbox(tmp_path, "pod_kill")
    sb.state("targets", "l-0 u0 n1\n")
    sb.state("live_before", "1\n")
    sb.state("crashed", "l-0 u0 n1 limiter c1 501 0\n")
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
        Rule(r"get pod l-0 -o json$", json.dumps(pod)),
        Rule(r"logs l-0 -c limiter --previous --timestamps --tail=1", f"{iso_ms(elder_last)} last words\n"),
        Rule(r"logs l-0 -c limiter --timestamps$", f"{iso_ms(successor_first)} first words\n{iso_ms(successor_first + 5)} more\n"),
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
    res = crashed_probe(tmp_path, 1, T0 + 1500, T0 + 900).run("probe")
    assert res.returncode == 1 and "logged 600 ms after its successor started" in res.stderr


def force_delete_probe(tmp_path: Path, elder: str) -> Sandbox:
    sb = Sandbox(tmp_path, "force_delete")
    sb.state("targets", "w-0 u0 n1\n")
    sb.state("live_before", "2\n")
    sb.state("crashed", "w-0 u0 n1 worker c1 77 0\n")
    sb.rules(
        Rule(r"docker exec n1 crictl inspect --output go-template --template \{\{\.status\.state\}\}", elder),
        Rule(r"docker exec n1 crictl logs --timestamps --tail 1 c1", f"{iso_ms(T0 + 2100)} still serving\n"),
        Rule(r"get pods -l app=b2bua-worker -o jsonpath=.*lastTransitionTime",
             "w-0 u9 True 2026-10-15T22:29:20Z 2026-10-15T22:29:10Z\nw-1 u1 True 2026-10-15T22:00:00Z 2026-10-15T22:00:00Z\n"),
        Rule(r"logs w-0 -c worker --timestamps$", f"{iso_ms(T0 + 300)} replacement up\n"),
    )
    return sb


def test_force_delete_leaves_the_process_and_deletes_the_pod(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "force_delete")
    sb.rules(
        Rule(r"get pods -l app=b2bua-worker -o jsonpath=.*spec\.nodeName", "w-0 u0 n1 True\n"),
        *crash_rules("w-0", "n1", [("worker", "c1", 77)]),
    )
    facts(sb.run("inject"))
    assert sb.index("delete pod w-0 --grace-period=0 --force") >= 0 and sb.index(" kill -9 ") == -1


def test_the_force_delete_probe_follows_the_elder_until_it_exits_and_measures_the_overlap(tmp_path: Path) -> None:
    assert force_delete_probe(tmp_path, "CONTAINER_RUNNING 0001-01-01T00:00:00Z").run("probe").returncode == 75
    sb = force_delete_probe(tmp_path, f"CONTAINER_EXITED {iso_ms(T0 + 2200)}")
    f = facts(sb.run("probe"))
    assert f["elders"] == [{
        "container": "w-0/worker", "elder_last_log_ms": T0 + 2100, "elder_exited_at_ms": T0 + 2200,
        "successor_first_log_ms": T0 + 300, "overlap_ms": 1800,
    }]
