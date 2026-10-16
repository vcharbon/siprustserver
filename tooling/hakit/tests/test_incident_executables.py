"""The base incident executables against fake `kubectl` and `docker`: each
does its fault to the pods it names, proves it, undoes it idempotently, and
never takes a failed read for an answer.

The rules state every answer the lab gives (an unscripted call exits 97);
the assertions read the calls made.
"""

from __future__ import annotations

import json
from pathlib import Path

from exe_kit import Rule, Sandbox, crash_rules, facts, failed, live

WORKERS = live("app=b2bua-worker", "w-0 u0 n1 True\n")
PARK = (
    "2026-10-15T22:29:10.500000Z  INFO b2bua::repl::supervisor: replication peer parked "
    "(left the membership set) node=w-1 peer=w-0\n"
)
PROXY_LOG = (
    "2026-10-15T22:29:10.100000Z  INFO sip_proxy::registry::projection: worker left the registry "
    'node="e1" worker=w-0 host=10.0.0.1\n'
    "2026-10-15T22:29:14.250000Z  INFO sip_proxy::registry::projection: worker joined the registry "
    'node="e1" worker=w-0 host=10.0.0.9\n'
)
T_RELEASE = 1_792_000_000_000


def iso(ms: int) -> str:
    from datetime import UTC, datetime

    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


def nodes_json(*tainted: str, clean: tuple[str, ...] = ()) -> str:
    items = [{"metadata": {"name": n}, "spec": {"taints": [{"key": "hakit.io/hold"}]}} for n in tainted]
    items += [{"metadata": {"name": n}, "spec": {"taints": [{"key": "other"}]}} for n in clean]
    return json.dumps({"items": items})


# ── kill_worker_held ──────────────────────────────────────────────────────


def held_inject(tmp_path: Path, restarts_at_delete: str) -> Sandbox:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.rules(
        WORKERS,
        Rule(r"get pod w-0 -o json$", '{"spec": {"nodeSelector": {"tier": "app"}}}'),
        Rule(r"get nodes -l tier=app ", "n1\nn2\n"),
        *crash_rules("w-0", "n1", [("worker", "c1")]),
        Rule(r"taint nodes n1 n2 hakit.io/hold=r-1:NoSchedule --overwrite$"),
        Rule(r"get pod w-0 --ignore-not-found -o jsonpath=\{range \.status\.containerStatuses", restarts_at_delete),
        Rule(r"delete pod w-0 --grace-period=0 --force --wait=false$"),
    )
    return sb


def test_a_held_kill_taints_then_crashes_then_deletes_the_pod(tmp_path: Path) -> None:
    sb = held_inject(tmp_path, "0\n")
    f = facts(sb.run("inject"))
    taint, kill, delete = sb.index("taint nodes n1 n2"), sb.index("task kill -s SIGKILL c1"), sb.index("delete pod w-0")
    assert 0 <= taint < kill < delete
    assert f["held_nodes"] == ["n1", "n2"] and f["containers"] == ["w-0/worker"] and f["restarts_at_delete"] == 0


def test_a_held_kill_fails_when_the_pod_restarted_in_place_before_its_delete(tmp_path: Path) -> None:
    failed(held_inject(tmp_path, "1\n").run("inject"), "restarted in place before its delete (0 -> 1 restarts)")


def test_heal_lifts_the_hold_proves_it_lifted_and_notes_the_waiting_replacement(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.state("targets", "w-0 u0 n1\n")
    sb.rules(
        Rule(r"get pod w-0 --ignore-not-found -o jsonpath=\{\.metadata\.uid\}", "u9 Pending"),
        Rule(r"get nodes -o json$", nodes_json("n1", clean=("n3",)), once=True),
        Rule(r"taint nodes n1 hakit.io/hold:NoSchedule-$"),
        Rule(r"get nodes -o json$", nodes_json(clean=("n1", "n3"))),
    )
    f = facts(sb.run("heal"))
    assert sb.lines(" taint ") == ["kubectl --context kind-lab taint nodes n1 hakit.io/hold:NoSchedule-"]
    assert len(sb.lines("get nodes -o json")) == 3  # before, after, and a second later
    assert (f["pending_uid"], f["pending_phase"]) == ("u9", "Pending")
    assert (sb.event_dir / "pending_uid").read_text().strip() == "u9"


def test_heal_fails_while_a_node_still_carries_the_hold(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.rules(Rule(r"get nodes -o json$", nodes_json("n2")), Rule(r"taint nodes n2 hakit.io/hold:NoSchedule-$"))
    failed(sb.run("heal"), "n2 still carries the hold taint")


def test_heal_fails_when_the_nodes_cannot_be_read(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.rules(Rule(r"get nodes -o json$", rc=1))
    res = sb.run("heal")
    assert res.returncode == 1 and "unscripted" not in res.stderr
    assert sb.index(" taint ") == -1 and not (sb.event_dir / "released_at_ms").exists()


def held_probe(tmp_path: Path, uid: str, scheduled_ms: int, old_present: bool = False) -> Sandbox:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.state("targets", "w-0 u0 n1\n")
    sb.state("live_before", "2\n")
    sb.state("pending_uid", "u9\n")
    sb.state("released_at_ms", f"{T_RELEASE}\n")
    sb.state("killed_at_ms", f"{T_RELEASE - 60_000}\n")
    t = iso(scheduled_ms)

    def pod(name: str, puid: str, ready: str = "True") -> dict[str, object]:
        return {"metadata": {"name": name, "uid": puid}, "status": {
            "conditions": [{"type": "Ready", "status": ready, "lastTransitionTime": t}],
            "containerStatuses": [{"state": {"running": {"startedAt": t}}}]}}

    items = [pod("w-0", uid), pod("w-1", "u1")] + ([pod("w-0x", "u0", "False")] if old_present else [])
    sb.rules(
        Rule(r"get pods -l app=b2bua-worker -o json$", json.dumps({"items": items})),
        Rule(r"get pod w-0 -o jsonpath=.*PodScheduled", f"{uid} {t}"),
        live("app=b2bua-worker", "w-0 u9 n1 True\nw-1 u1 n2 True\n"),
        live("app=sip-front-proxy", "p-0 pu e1 True\n"),
        Rule(r"logs w-1 --since-time=", PARK),
        Rule(r"logs p-0 -c sip-front-proxy --since-time=", PROXY_LOG),
    )
    return sb


def test_the_held_probe_proves_one_incarnation_after_the_release(tmp_path: Path) -> None:
    f = facts(held_probe(tmp_path, "u9", T_RELEASE + 2000).run("probe"))
    assert f["incarnations"] == 1 and f["released_at_ms"] == T_RELEASE
    assert f["survivors"] == [{"pod": "w-1", "parked_at_ms": 1792103350500}]
    assert f["proxies"] == [{"pod": "p-0", "left_at_ms": 1792103350100, "joined_at_ms": 1792103354250}]


def test_the_held_probe_fails_on_a_replacement_other_than_the_one_that_waited(tmp_path: Path) -> None:
    failed(held_probe(tmp_path, "u8", T_RELEASE + 2000).run("probe"), "not the replacement u9")


def test_the_held_probe_fails_on_a_replacement_scheduled_during_the_hold(tmp_path: Path) -> None:
    failed(held_probe(tmp_path, "u9", T_RELEASE - 30_000).run("probe"), "before the release")


def test_the_held_probe_waits_while_the_killed_pod_remains(tmp_path: Path) -> None:
    assert held_probe(tmp_path, "u9", T_RELEASE + 2000, old_present=True).run("probe").returncode == 75


# ── kill_proxy / cut_external_plane ───────────────────────────────────────

PROXIES = live("app=sip-front-proxy", "p-0 u0 e1 True\np-1 u1 e2 True\n")
HOLDER = Rule(r"exec p-1 -c keepalived -- ip", "1: lo    inet 127.0.0.1/8 scope host lo\n4: eth1    inet 192.168.60.250/32 scope global eth1\n")
BACKUP = Rule(r"exec p-0 -c keepalived -- ip", "1: lo    inet 192.168.60.250/32 scope host lo\n")


def test_kill_proxy_crashes_every_container_of_the_address_holder_and_not_its_backup(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy")
    sb.rules(PROXIES, HOLDER, BACKUP, *crash_rules("p-1", "e2", [("proxy", "c1"), ("keepalived", "c2")]))
    f = facts(sb.run("inject"))
    assert sb.lines("task kill") == [
        "docker exec e2 ctr -n k8s.io task kill -s SIGKILL c1",
        "docker exec e2 ctr -n k8s.io task kill -s SIGKILL c2",
    ]
    assert (f["pods"], f["node"], f["interface"], f["live_before"]) == (["p-1"], "e2", "eth1", 2)


def test_kill_proxy_probe_waits_while_the_addresses_cannot_be_read(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy")
    sb.state("crashed", "p-1 u1 e2 proxy c1 0\n")
    pod = {"metadata": {"uid": "u1"}, "status": {
        "conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": "2026-10-15T22:29:20Z"}],
        "containerStatuses": [{"name": "proxy", "restartCount": 1, "state": {"running": {"startedAt": "2026-10-15T22:29:12Z"}}}]}}
    sb.rules(
        Rule(r"get pod p-1 --ignore-not-found -o json$", json.dumps(pod)),
        Rule(r"logs p-1 -c proxy --previous --timestamps --tail=1$", ""),
        Rule(r"logs p-1 -c proxy --timestamps --limit-bytes=8192$", ""),
        PROXIES, BACKUP, Rule(r"exec p-1 -c keepalived -- ip", rc=1),
    )
    assert sb.run("probe").returncode == 75


def test_cut_external_plane_disconnects_the_holders_node_and_keeps_its_address(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.rules(PROXIES, HOLDER, BACKUP, Rule(r"docker inspect -f .*IPAddress.* e2$", "192.168.60.12\n"),
             Rule(r"docker network disconnect sipext e2$"))
    f = facts(sb.run("inject"))
    assert sb.index("docker network disconnect sipext e2") >= 0
    assert (sb.event_dir / "cut").read_text().split() == ["e2", "192.168.60.12"]
    assert f["node"] == "e2"


def test_cut_heal_reconnects_at_the_kept_address_only_when_disconnected(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.state("cut", "e2 192.168.60.12\n")
    sb.rules(Rule(r"docker inspect", "\n", once=True), Rule(r"docker network connect --ip 192.168.60.12 sipext e2$"),
             Rule(r"docker inspect", "192.168.60.12\n"))
    facts(sb.run("heal"))
    assert sb.index("docker network connect --ip 192.168.60.12 sipext e2") >= 0
    assert len(sb.lines("docker inspect")) == 3  # before, after, and a second later
    sb.rules(Rule(r"docker inspect", "192.168.60.12\n"))
    facts(sb.run("heal"))
    assert sb.index("network connect") == -1


def test_cut_heal_fails_when_the_node_is_not_back_at_its_address(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.state("cut", "e2 192.168.60.12\n")
    sb.rules(Rule(r"docker inspect", "\n", once=True), Rule(r"docker network connect"), Rule(r"docker inspect", "192.168.60.99\n"))
    failed(sb.run("heal"), "as '192.168.60.99', not 192.168.60.12")
