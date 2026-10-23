"""The held pod and node faults (lib/timed.sh, cpu_starve, node_stop) against
fake `kubectl` and `docker`: applied and proven, undone and proven twice, a
replaced pod left alone, and a read that fails never taken for "gone"."""

from __future__ import annotations

from pathlib import Path

from exe_kit import Rule, Sandbox, facts, failed, live, pod_rules

LIMITER = live("app=call-limiter", "l-0 u0 n1 True\n")
FREEZE_WRITE = "docker exec n1 sh -c echo {} > '/sys/fs/cgroup/kubepods/pod1/cgroup.freeze'"


def freeze_heal(tmp_path: Path, uid: str = "u0", frozen: str | list[str] = "0") -> Sandbox:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.state("targets", "l-0 u0 n1\n")
    sb.state("restarts_before", '{"l-0": 0}')
    sb.rules(*pod_rules("l-0", "n1", uid, frozen=frozen))
    return sb


def test_freeze_freezes_the_pod_level_cgroup_and_proves_it(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.rules(LIMITER, *pod_rules("l-0", "n1", "u0", frozen="1"))
    f = facts(sb.run("inject"))
    assert sb.index(FREEZE_WRITE.format(1)) >= 0
    assert f["pods"] == ["l-0"] and f["restarts_before"] == {"l-0": 0}


def test_freeze_inject_fails_when_the_freeze_does_not_take(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.rules(LIMITER, *pod_rules("l-0", "n1", "u0", frozen="0"))
    failed(sb.run("inject"), "pf_freeze on l-0 failed")


def test_freeze_heal_thaws_the_same_pod_twice_and_proves_it_thawed(tmp_path: Path) -> None:
    sb = freeze_heal(tmp_path)
    f = facts(sb.run("heal"))
    assert len(sb.lines(FREEZE_WRITE.format(0))) == 2  # the heal and its re-check a second later
    assert f["pods"] == {"l-0": {"state": "healed", "restarts": 0}}


def test_freeze_heal_fails_when_the_pod_stays_frozen(tmp_path: Path) -> None:
    failed(freeze_heal(tmp_path, frozen="1").run("heal"), "pf_thaw on l-0 failed")


def test_freeze_heal_leaves_a_replaced_pod_alone(tmp_path: Path) -> None:
    sb = freeze_heal(tmp_path, uid="u7")
    f = facts(sb.run("heal"))
    assert sb.index("cgroup.freeze") == -1
    assert f["pods"] == {"l-0": {"state": "replaced", "restarts": None}}


def test_freeze_heal_fails_when_the_pod_cannot_be_read(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "pod_freeze")
    sb.state("targets", "l-0 u0 n1\n")
    sb.rules(Rule(r"get pod l-0 --ignore-not-found -o jsonpath=\{\.metadata\.uid\}$", rc=1))
    res = sb.run("heal")
    assert res.returncode == 1 and "unscripted" not in res.stderr and res.stdout == ""


def test_tcp_reset_heal_deletes_every_reset_rule_and_proves_none_left(tmp_path: Path) -> None:
    rule = "-A INPUT -p tcp -m tcp --dport 8080 -m comment --comment hakit-reject -j REJECT --reject-with tcp-reset"
    sb = Sandbox(tmp_path, "tcp_reset")
    sb.state("targets", "l-0 u0 n1\n")
    sb.rules(
        Rule(r"nsenter -t 111 -n -- iptables -D INPUT -p tcp -m tcp --dport 8080 -m comment --comment hakit-reject"),
        *pod_rules("l-0", "n1", "u0", iptables=[f"-P INPUT ACCEPT\n{rule}\n", "-P INPUT ACCEPT\n"]),
    )
    f = facts(sb.run("heal"))
    assert len(sb.lines("iptables -D INPUT")) == 1
    assert f["pods"] == {"l-0": {"state": "healed", "restarts": None}}


def test_a_netns_fault_refuses_a_host_network_pod_unless_allowed(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "net_delay")
    sb.rules(LIMITER, *pod_rules("l-0", "n1", "u0", host_network="true"))
    failed(sb.run("inject"), "l-0 is a host-network pod")
    assert sb.index(" tc qdisc replace") == -1
    allowed = Sandbox(tmp_path, "net_delay", allow_host_network="true")
    allowed.rules(LIMITER, Rule(r"tc qdisc replace dev eth0 root netem delay 300ms$"),
                  *pod_rules("l-0", "n1", "u0", host_network="true", qdisc="qdisc netem 8001: root"))
    facts(allowed.run("inject"))


def test_sip_loss_impairs_only_the_sip_port(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "sip_loss", loss_pct="7", duplicate_pct="3", delay_ms="40")
    sb.rules(live("app=b2bua-worker", "w-0 u0 n1 True\n"), Rule(r"tc qdisc replace"), Rule(r"tc filter add"),
             *pod_rules("w-0", "n1", "u0", qdisc="qdisc netem 40: parent 1:4"))
    facts(sb.run("inject"))
    tc = [c.argv[c.argv.index("tc"):] for c in sb.calls() if "tc" in c.argv and "show" not in c.argv]
    assert tc[1] == ["tc", "qdisc", "replace", "dev", "eth0", "parent", "1:4", "handle", "40:", "netem",
                     "loss", "7%", "duplicate", "3%", "delay", "40ms"]
    filters = [" ".join(t) for t in tc if t[1] == "filter"]
    assert len(filters) == 2
    assert all("match ip protocol 17 0xff" in f and "5060 0xffff flowid 1:4" in f for f in filters)
    assert {"sport", "dport"} == {f.split("match ip ")[2].split()[0] for f in filters}


# ── cpu_starve ────────────────────────────────────────────────────────────


def test_cpu_starve_caps_the_pod_level_cgroup_and_keeps_the_prior_value(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cpu_starve", quota_us="20000")
    sb.rules(
        live("app=b2bua-worker", "w-0 u0 n1 True\n"),
        Rule(r"docker exec n1 cat /sys/fs/cgroup/kubepods/pod1/cpu.max$", "max 100000\n", once=True),
        Rule(r"docker exec n1 sh -c echo '20000 100000' > '/sys/fs/cgroup/kubepods/pod1/cpu.max'$"),
        Rule(r"docker exec n1 cat /sys/fs/cgroup/kubepods/pod1/cpu.max$", "20000 100000\n"),
        *pod_rules("w-0", "n1", "u0"),
    )
    f = facts(sb.run("inject"))
    assert (sb.event_dir / "starved").read_text() == "w-0 n1 /sys/fs/cgroup/kubepods/pod1 max 100000\n"
    assert f["pods"] == {"w-0": {"cpu_max": "20000 100000", "was": "max 100000"}}


def test_cpu_starve_heal_writes_back_the_prior_cap_and_leaves_a_gone_cgroup(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cpu_starve")
    sb.state("starved", "w-0 n1 /sys/fs/cgroup/x max 100000\nw-1 n2 /sys/fs/cgroup/y max 100000\n")
    sb.rules(
        Rule(r"docker exec n1 test -d /sys/fs/cgroup/x$"),
        Rule(r"docker exec n1 sh -c echo 'max 100000' > '/sys/fs/cgroup/x/cpu.max'$"),
        Rule(r"docker exec n1 cat /sys/fs/cgroup/x/cpu.max$", "max 100000\n"),
        Rule(r"docker exec n2 test -d /sys/fs/cgroup/y$", rc=1),
        Rule(r"docker exec n2 true$"),
    )
    f = facts(sb.run("heal"))
    assert len(sb.lines("echo 'max 100000' > '/sys/fs/cgroup/x/cpu.max'")) == 2
    assert f["pods"] == {"w-0": "restored", "w-1": "gone"}


def test_cpu_starve_heal_fails_when_the_node_cannot_be_reached(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cpu_starve")
    sb.state("starved", "w-1 n2 /sys/fs/cgroup/y max 100000\n")
    sb.rules(Rule(r"docker exec n2 test -d", rc=125), Rule(r"docker exec n2 true$", rc=125))
    failed(sb.run("heal"), "node n2 cannot be reached")


# ── node_stop ─────────────────────────────────────────────────────────────

RABBIT = live("app=rabbitmq", "r-0 u0 infra1 True\n")


def test_node_stop_never_stops_a_control_plane_node(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "node_stop")
    sb.rules(live("app=rabbitmq", "r-0 u0 cp True\n"),
             Rule(r"get node cp -o jsonpath=\{\.metadata\.labels\}", '{"node-role.kubernetes.io/control-plane": ""}'))
    failed(sb.run("inject"), "cp is a control-plane node")
    assert sb.index("docker stop") == -1


def test_node_stop_stops_the_node_of_the_target(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "node_stop")
    sb.rules(
        RABBIT,
        Rule(r"get node infra1 -o jsonpath=\{\.metadata\.labels\}", '{"tier": "infra"}'),
        Rule(r"get pods --field-selector spec.nodeName=infra1 -o json", '{"items": [{"metadata": {"name": "r-0"}}]}'),
        Rule(r"docker stop -t 0 infra1$"),
    )
    f = facts(sb.run("inject"))
    assert (f["nodes"], f["pods"]) == (["infra1"], ["r-0"])


def test_node_stop_stops_the_nodes_its_node_selector_names(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "node_stop", node_selector="tier=app")
    sb.rules(
        RABBIT,
        Rule(r"get nodes -l tier=app -o jsonpath", "a1\na2\n"),
        Rule(r"get node a[12] -o jsonpath=\{\.metadata\.labels\}", '{"tier": "app"}'),
        Rule(r"get pods --field-selector spec.nodeName=a[12] -o json", '{"items": []}'),
        Rule(r"docker stop -t 0 a[12]$"),
    )
    f = facts(sb.run("inject"))
    assert sb.lines("docker stop") == ["docker stop -t 0 a1", "docker stop -t 0 a2"]
    assert f["nodes"] == ["a1", "a2"]


def node_heal(tmp_path: Path, running: str = "true") -> Sandbox:
    sb = Sandbox(tmp_path, "node_stop")
    sb.state("nodes", "a1\na2\n")
    sb.rules(
        Rule(r"get node a[12] -o jsonpath=\{\.status\.conditions", "Unknown"),
        Rule(r"docker start a[12]$"),
        Rule(r"wait --for=condition=Ready node/a[12] --timeout="),
        Rule(r"docker inspect -f \{\{\.State\.Running\}\} a[12]$", running),
    )
    return sb


def test_node_stop_heal_starts_every_node_before_waiting_on_any(tmp_path: Path) -> None:
    sb = node_heal(tmp_path)
    f = facts(sb.run("heal"))
    assert sb.index("docker start a1") < sb.index("docker start a2") < sb.index("node/a1")
    assert f["nodes"] == {"a1": {"not_ready_seen": True, "ready_at_ms": f["nodes"]["a1"]["ready_at_ms"]},  # type: ignore[index]
                          "a2": {"not_ready_seen": True, "ready_at_ms": f["nodes"]["a2"]["ready_at_ms"]}}  # type: ignore[index]


def test_node_stop_heal_fails_when_a_node_is_not_running_a_second_later(tmp_path: Path) -> None:
    failed(node_heal(tmp_path, running="false").run("heal"), "a1 is not running a second after its start")
