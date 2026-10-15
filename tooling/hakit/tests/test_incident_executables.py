"""The base incident executables against fake `kubectl` and `docker`: each
does its fault to the pods it names, proves it, and undoes it idempotently.

The rules state what the lab answers; the assertions read the calls made.
"""

from __future__ import annotations

import json
from pathlib import Path

from exe_kit import Rule, Sandbox, facts

WORKERS = Rule(r"get pods -l app=b2bua-worker -o jsonpath=.*spec\.nodeName", "w-0 u0 n1 True\n")
REPLACED = r"get pods -l app=b2bua-worker -o jsonpath=.*lastTransitionTime"
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
T_RELEASE = 1_792_000_000_000  # 2026-10-15T...:00Z, the release instant of the held kill


def iso(ms: int) -> str:
    from datetime import UTC, datetime

    return datetime.fromtimestamp(ms / 1000, UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


# ── kill_worker_held ──────────────────────────────────────────────────────


def test_a_held_kill_taints_the_admitting_nodes_before_the_kill(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.rules(
        WORKERS,
        Rule(r"get pod w-0 -o json$", '{"spec": {"nodeSelector": {"tier": "app"}}}'),
        Rule(r"get nodes -l tier=app ", "n1\nn2\n"),
    )
    f = facts(sb.run("inject"))
    taint, kill = sb.index("taint nodes n1 n2 hakit.io/hold=r-1:NoSchedule"), sb.index("delete pod w-0")
    assert 0 <= taint < kill
    assert "--grace-period=0 --force" in sb.calls()[kill].line
    assert f["held_nodes"] == ["n1", "n2"] and f["pods"] == ["w-0"]


def test_heal_lifts_the_hold_from_every_tainted_node_and_notes_the_waiting_replacement(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.state("targets", "w-0 u0 n1\n")
    nodes = {
        "items": [
            {"metadata": {"name": "n1"}, "spec": {"taints": [{"key": "hakit.io/hold"}]}},
            {"metadata": {"name": "n3"}, "spec": {"taints": [{"key": "other"}]}},
        ]
    }
    sb.rules(Rule(r"get pod w-0 -o jsonpath", "u9 Pending"), Rule(r"get nodes -o json", json.dumps(nodes)))
    f = facts(sb.run("heal"))
    untaints = [c.line for c in sb.calls() if " taint " in c.line]
    assert untaints == ["kubectl --context kind-lab taint nodes n1 hakit.io/hold:NoSchedule-"]
    assert (f["pending_uid"], f["pending_phase"]) == ("u9", "Pending")
    assert (sb.event_dir / "pending_uid").read_text().strip() == "u9"


def test_heal_after_a_failed_inject_still_lifts_the_hold(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker_held")
    nodes = {"items": [{"metadata": {"name": "n2"}, "spec": {"taints": [{"key": "hakit.io/hold"}]}}]}
    sb.rules(Rule(r"get nodes -o json", json.dumps(nodes)))
    f = facts(sb.run("heal"))
    assert sb.index("taint nodes n2 hakit.io/hold:NoSchedule-") >= 0
    assert f["pending_uid"] is None


def held_probe(tmp_path: Path, uid: str, scheduled_ms: int, old_present: bool = False) -> Sandbox:
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.state("targets", "w-0 u0 n1\n")
    sb.state("live_before", "2\n")
    sb.state("pending_uid", "u9\n")
    sb.state("released_at_ms", f"{T_RELEASE}\n")
    sb.state("deleted_at_ms", f"{T_RELEASE - 60_000}\n")
    t = iso(scheduled_ms)
    rows = f"w-0 {uid} True {t} {t}\nw-1 u1 True {t} {t}\n" + ("w-0x u0 False\n" if old_present else "")
    sb.rules(
        Rule(REPLACED, rows),
        Rule(r"get pod w-0 -o jsonpath=.*PodScheduled", f"{uid} {t}"),
        Rule(r"get pods -l app=b2bua-worker -o jsonpath=.*spec\.nodeName", "w-0 u9 n1 True\nw-1 u1 n2 True\n"),
        Rule(r"get pods -l app=sip-front-proxy -o jsonpath=.*spec\.nodeName", "p-0 pu e1 True\n"),
        Rule(r"logs w-1 ", PARK),
        Rule(r"logs p-0 -c sip-front-proxy ", PROXY_LOG),
    )
    return sb


def test_the_held_probe_proves_one_incarnation_after_the_release(tmp_path: Path) -> None:
    sb = held_probe(tmp_path, "u9", T_RELEASE + 2000)
    f = facts(sb.run("probe"))
    assert f["incarnations"] == 1 and f["released_at_ms"] == T_RELEASE
    assert f["survivors"] == [{"pod": "w-1", "parked_at_ms": 1792103350500}]
    assert f["proxies"] == [{"pod": "p-0", "left_at_ms": 1792103350100, "joined_at_ms": 1792103354250}]


def test_the_held_probe_fails_on_a_replacement_other_than_the_one_that_waited(tmp_path: Path) -> None:
    res = held_probe(tmp_path, "u8", T_RELEASE + 2000).run("probe")
    assert res.returncode == 1 and "not the replacement u9" in res.stderr


def test_the_held_probe_fails_on_a_replacement_scheduled_during_the_hold(tmp_path: Path) -> None:
    res = held_probe(tmp_path, "u9", T_RELEASE - 30_000).run("probe")
    assert res.returncode == 1 and "before the release" in res.stderr


def test_the_held_probe_waits_while_the_killed_pod_remains(tmp_path: Path) -> None:
    assert held_probe(tmp_path, "u9", T_RELEASE + 2000, old_present=True).run("probe").returncode == 75


# ── kill_worker: restart timings ──────────────────────────────────────────


def test_the_kill_probe_waits_until_as_many_pods_are_live_as_before(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_worker")
    sb.state("targets", "w-0 u0 n1\n")
    sb.state("live_before", "2\n")
    sb.rules(Rule(REPLACED, "w-0 u9 False\nw-1 u1 True 2026-10-15T22:29:10Z 2026-10-15T22:29:00Z\n"))
    assert sb.run("probe").returncode == 75


# ── kill_proxy / cut_external_plane ───────────────────────────────────────

PROXIES = Rule(r"get pods -l app=sip-front-proxy -o jsonpath=.*spec\.nodeName", "p-0 u0 e1 True\np-1 u1 e2 True\n")
HOLDER = Rule(r"exec p-1 -c keepalived -- ip", "1: lo    inet 127.0.0.1/8 scope host lo\n4: eth1    inet 192.168.60.250/32 scope global eth1\n")
BACKUP = Rule(r"exec p-0 -c keepalived -- ip", "1: lo    inet 192.168.60.250/32 scope host lo\n")


def test_kill_proxy_deletes_the_address_holder_and_not_its_backup(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy")
    sb.rules(PROXIES, HOLDER, BACKUP)
    f = facts(sb.run("inject"))
    deletes = [c.line for c in sb.calls() if " delete pod " in c.line]
    assert len(deletes) == 1 and "delete pod p-1 --grace-period=0 --force" in deletes[0]
    assert (f["pods"], f["node"], f["interface"], f["live_before"]) == (["p-1"], "e2", "eth1", 2)


def test_cut_external_plane_disconnects_the_holders_node_and_keeps_its_address(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.rules(PROXIES, HOLDER, BACKUP, Rule(r"docker inspect -f .*IPAddress.* e2", "192.168.60.12\n"))
    f = facts(sb.run("inject"))
    assert sb.index("docker network disconnect sipext e2") >= 0
    assert (sb.event_dir / "cut").read_text().split() == ["e2", "192.168.60.12"]
    assert f["node"] == "e2"


def test_cut_heal_reconnects_at_the_kept_address_only_when_disconnected(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.state("cut", "e2 192.168.60.12\n")
    sb.rules(Rule(r"docker inspect", "\n", once=True), Rule(r"docker inspect", "192.168.60.12\n"))
    facts(sb.run("heal"))
    assert sb.index("docker network connect --ip 192.168.60.12 sipext e2") >= 0
    sb.rules(Rule(r"docker inspect", "192.168.60.12\n"))
    facts(sb.run("heal"))
    assert sb.index("network connect") == -1


# ── timed pod faults ──────────────────────────────────────────────────────


def lab_of(pod: str, uid: str, frozen: str = "0", qdisc: str = "qdisc noqueue 0: root") -> list[Rule]:
    """The node-side reads pod-faults.sh makes for one pod on node n1."""
    return [
        Rule(rf"get pod {pod} -o jsonpath={{\.metadata\.uid}}$", uid),
        Rule(rf"get pod {pod} -o jsonpath={{\.spec\.nodeName}}", "n1"),
        Rule(rf"get pod {pod} -o jsonpath={{\.status\.containerStatuses\[0\]\.containerID}}", "containerd://c1"),
        Rule(rf"get pod {pod} -o jsonpath={{range \.status\.containerStatuses", "0\n"),
        Rule(r"docker exec n1 crictl pods", "sb1\n"),
        Rule(r"docker exec n1 crictl inspectp", "111"),
        Rule(r"docker exec n1 crictl inspect ", "222"),
        Rule(r"docker exec n1 cat /proc/222/cgroup", "0::/kubepods/c1\n"),
        Rule(r"docker exec n1 cat /sys/fs/cgroup/kubepods/c1/cgroup\.freeze", frozen),
        Rule(r"nsenter -t 111 -n -- tc qdisc show", qdisc),
        Rule(r"nsenter -t 111 -n -- iptables -S INPUT", "-P INPUT ACCEPT\n"),
    ]


def test_freeze_heal_thaws_the_same_pod_and_proves_it_thawed(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.state("targets", "l-0 u0 n1\n")
    sb.state("restarts_before", '{"l-0": 0}')
    sb.rules(*lab_of("l-0", "u0"))
    f = facts(sb.run("heal"))
    assert sb.index("echo 0 > '/sys/fs/cgroup/kubepods/c1/cgroup.freeze'") >= 0
    assert f["pods"] == {"l-0": {"state": "healed", "restarts": 0}}


def test_freeze_heal_fails_when_the_pod_stays_frozen(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.state("targets", "l-0 u0 n1\n")
    sb.rules(*lab_of("l-0", "u0", frozen="1"))
    res = sb.run("heal")
    assert res.returncode == 1 and "frozen is still on l-0" in res.stderr


def test_freeze_heal_leaves_a_replaced_pod_alone(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.state("targets", "l-0 u0 n1\n")
    sb.rules(*lab_of("l-0", "u7"))
    f = facts(sb.run("heal"))
    assert sb.index("cgroup.freeze") == -1
    assert f["pods"] == {"l-0": {"state": "replaced", "restarts": None}}


def test_freeze_inject_fails_when_the_freeze_does_not_take(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.rules(Rule(r"get pods -l app=call-limiter -o jsonpath=.*spec\.nodeName", "l-0 u0 n1 True\n"), *lab_of("l-0", "u0"))
    res = sb.run("inject")
    assert res.returncode == 1 and "frozen is not in place on l-0" in res.stderr


def test_sip_loss_impairs_only_the_sip_port(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "sip_loss", loss_pct="7", duplicate_pct="3", delay_ms="40")
    sb.rules(WORKERS, *lab_of("w-0", "u0", qdisc="qdisc netem 40: parent 1:4"))
    facts(sb.run("inject"))
    tc = [c.argv[c.argv.index("tc"):] for c in sb.calls() if "tc" in c.argv and "show" not in c.argv]
    assert tc[1] == ["tc", "qdisc", "replace", "dev", "eth0", "parent", "1:4", "handle", "40:", "netem",
                     "loss", "7%", "duplicate", "3%", "delay", "40ms"]
    filters = [" ".join(t) for t in tc if t[1] == "filter"]
    assert len(filters) == 2
    assert all("match ip protocol 17 0xff" in f and "5060 0xffff flowid 1:4" in f for f in filters)
    assert {"sport", "dport"} == {f.split("match ip ")[2].split()[0] for f in filters}


# ── cpu_starve ────────────────────────────────────────────────────────────


def test_cpu_starve_heal_writes_back_the_prior_cap(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cpu_starve")
    sb.state("starved", "w-0 n1 /sys/fs/cgroup/x max 100000\nw-1 n2 /sys/fs/cgroup/y max 100000\n")
    sb.rules(
        Rule(r"docker exec n2 test -f", rc=1),
        Rule(r"docker exec n1 cat /sys/fs/cgroup/x/cpu.max", "max 100000\n"),
    )
    f = facts(sb.run("heal"))
    assert sb.index("echo 'max 100000' > '/sys/fs/cgroup/x/cpu.max'") >= 0
    assert sb.index("/sys/fs/cgroup/y/cpu.max'") == -1
    assert f["pods"] == {"w-0": "restored", "w-1": "gone"}


def test_cpu_starve_inject_proves_the_cap_and_keeps_the_prior_value(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cpu_starve", quota_us="20000")
    sb.rules(
        WORKERS,
        Rule(r"get pod w-0 -o jsonpath=\{\.status\.containerStatuses\[0\]", "containerd://abc"),
        Rule(r"docker exec n1 find /sys/fs/cgroup", "/sys/fs/cgroup/kp/abc\n"),
        Rule(r"docker exec n1 cat /sys/fs/cgroup/kp/abc/cpu.max", "max 100000\n", once=True),
        Rule(r"docker exec n1 cat /sys/fs/cgroup/kp/abc/cpu.max", "20000 100000\n"),
    )
    f = facts(sb.run("inject"))
    assert (sb.event_dir / "starved").read_text() == "w-0 n1 /sys/fs/cgroup/kp/abc max 100000\n"
    assert f["pods"] == {"w-0": {"cpu_max": "20000 100000", "was": "max 100000"}}


# ── node_stop ─────────────────────────────────────────────────────────────


def test_node_stop_never_stops_a_control_plane_node(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "node_stop")
    sb.rules(
        Rule(r"get pods -l app=rabbitmq -o jsonpath=.*spec\.nodeName", "r-0 u0 cp True\n"),
        Rule(r"get node cp -o jsonpath=\{\.metadata\.labels\}", '{"node-role.kubernetes.io/control-plane": ""}'),
    )
    res = sb.run("inject")
    assert res.returncode == 1 and sb.index("docker stop") == -1


def test_node_stop_stops_the_node_of_the_target(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "node_stop")
    sb.rules(
        Rule(r"get pods -l app=rabbitmq -o jsonpath=.*spec\.nodeName", "r-0 u0 infra1 True\n"),
        Rule(r"get node infra1 -o jsonpath=\{\.metadata\.labels\}", '{"tier": "infra"}'),
        Rule(r"get pods --field-selector spec.nodeName=infra1 -o json", '{"items": [{"metadata": {"name": "r-0"}}]}'),
    )
    f = facts(sb.run("inject"))
    assert sb.index("docker stop -t 0 infra1") >= 0
    assert (f["nodes"], f["pods"]) == (["infra1"], ["r-0"])


# ── Chaos Mesh ────────────────────────────────────────────────────────────


def test_partition_applies_a_both_way_partition_and_waits_for_its_injection(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "partition")
    records = {"status": {"experiment": {"containerRecords": [
        {"id": "ns/w-0", "selectorKey": "."}, {"id": "ns/w-1", "selectorKey": ".Target"}]}}}
    sb.rules(
        Rule(r"get pods -l app=b2bua-worker -o json", '{"items": [{"spec": {}}]}'),
        Rule(r"get networkchaos hakit-001-partition-partition -o jsonpath", "", once=True),
        Rule(r"get networkchaos hakit-001-partition-partition -o jsonpath", "True"),
        Rule(r"get networkchaos hakit-001-partition-partition -o json$", json.dumps(records)),
    )
    f = facts(sb.run("inject"))
    applied = json.loads(next(c.stdin for c in sb.calls() if c.argv[-3:] == ["apply", "-f", "-"]))
    assert applied["metadata"] == {"name": "hakit-001-partition-partition", "namespace": "ns", "labels": {"hakit.io/run": "r-1"}}
    sel = {"namespaces": ["ns"], "labelSelectors": {"app": "b2bua-worker"}}
    assert applied["spec"] == {"action": "partition", "mode": "one", "selector": sel, "direction": "both",
                               "target": {"mode": "all", "selector": sel}}
    assert f["pods"] == ["w-0"]


def test_partition_refuses_host_network_pods_before_applying_anything(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "partition")
    sb.rules(Rule(r"get pods -l app=b2bua-worker -o json", '{"items": [{"spec": {"hostNetwork": true}}]}'))
    res = sb.run("inject")
    assert res.returncode == 1 and sb.index(" apply ") == -1


def test_a_chaos_mesh_heal_drops_the_finalizer_only_of_a_resource_that_injected_nothing(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "clock_skew")
    nothing = {"status": {"experiment": {"containerRecords": [{"phase": "Not Injected"}]}}}
    sb.rules(Rule(r"delete timechaos", rc=1), Rule(r"get timechaos .* -o json", json.dumps(nothing)))
    facts(sb.run("heal"))
    assert sb.index('patch timechaos hakit-001-clock-skew-clock-skew --type=merge') >= 0
    injected = {"status": {"experiment": {"containerRecords": [{"phase": "Injected"}]}}}
    sb.rules(Rule(r"delete timechaos", rc=1), Rule(r"get timechaos .* -o json", json.dumps(injected)))
    res = sb.run("heal")
    assert res.returncode == 1 and sb.index(" patch ") == -1


def test_io_stall_delays_the_volume_it_names(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "io_stall", delay_ms="250", container="rabbitmq")
    sb.rules(Rule(r"get iochaos .* -o jsonpath", "True"), Rule(r"get iochaos .* -o json$", "{}"))
    facts(sb.run("inject"))
    applied = json.loads(next(c.stdin for c in sb.calls() if c.argv[-3:] == ["apply", "-f", "-"]))
    assert applied["kind"] == "IOChaos"
    assert {k: applied["spec"][k] for k in ("action", "volumePath", "path", "delay", "percent", "containerNames")} == {
        "action": "latency", "volumePath": "/var/lib/rabbitmq", "path": "/var/lib/rabbitmq/**",
        "delay": "250ms", "percent": 100, "containerNames": ["rabbitmq"]}
