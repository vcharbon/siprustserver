"""The base incident executables against fake `kubectl` and `docker`: each
does its fault to the pods it names, proves it, undoes it idempotently, and
never takes a failed read for an answer.

The rules state every answer the lab gives (an unscripted call exits 97);
the assertions read the calls made.
"""

from __future__ import annotations

import json
from pathlib import Path

from hakit.jsonval import Json, as_obj, number, objs

from exe_kit import Rule, Sandbox, crash_rules, facts, failed, live, plane_rules, pod_rules, route_rule

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
    # The delete follows the signal at once: the death is proven after it, so
    # the kubelet's next relist finds no pod to restart in place.
    inspects = [i for i, c in enumerate(sb.calls()) if "crictl inspect" in c.line and "c1" in c.line]
    assert inspects[-1] > delete
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


def test_the_release_instant_precedes_the_untaint(tmp_path: Path) -> None:
    """The scheduler may place the waiting pod the moment the first taint goes,
    so the release instant the probe compares with is taken before it."""
    sb = Sandbox(tmp_path, "kill_worker_held")
    sb.state("targets", "w-0 u0 n1\n")
    sb.rules(
        Rule(r"get pod w-0 --ignore-not-found -o jsonpath=\{\.metadata\.uid\}", "u9 Pending"),
        Rule(r"get nodes -o json$", nodes_json("n1"), once=True),
        Rule(r"taint nodes n1 hakit.io/hold:NoSchedule-$"),
        Rule(r"get nodes -o json$", nodes_json(clean=("n1",))),
    )
    f = facts(sb.run("heal"))
    [untaint] = [c for c in sb.calls() if " taint " in c.line]
    assert int(str(f["released_at_ms"])) <= untaint.at_ms


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


# ── front proxy routes: kill_proxy, cut_external_plane, proxy_rollout,
#    speaker_restart, backend_outage, node_stop ───────────────────────────

def withdrawn(f: Json) -> set[str]:
    """The "vip addr" pairs an inject timed the withdrawal of."""
    return set(as_obj(f["withdrawn_ms"]))


IVIP, EVIP = "172.20.255.250", "192.168.60.250"
PROXIES = live("app=sip-front-proxy", "p-0 u0 e1 True\np-1 u1 e2 True\n")
PLANES = [*plane_rules("e1", "172.20.0.3", "192.168.60.11"), *plane_rules("e2", "172.20.0.4", "192.168.60.12")]
BOTH = [route_rule(IVIP, "172.20.0.3", "172.20.0.4"), route_rule(EVIP, "192.168.60.11", "192.168.60.12")]
E1_ONLY = [route_rule(IVIP, "172.20.0.3"), route_rule(EVIP, "192.168.60.11")]
ONE_PROXY = live("app=sip-front-proxy", "p-1 u1 e2 True\n")


def test_kill_proxy_crashes_one_proxy_and_times_its_nodes_withdrawal(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy")
    sb.rules(ONE_PROXY, *PLANES,
             route_rule(IVIP, "172.20.0.3", "172.20.0.4", once=True),
             route_rule(EVIP, "192.168.60.11", "192.168.60.12", once=True), *E1_ONLY,
             *crash_rules("p-1", "e2", [("proxy", "c1")]))
    f = facts(sb.run("inject"))
    assert sb.lines("task kill") == ["docker exec e2 ctr -n k8s.io task kill -s SIGKILL c1"]
    assert (f["pods"], f["node"], f["live_before"]) == (["p-1"], "e2", 1)
    assert withdrawn(f) == {f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}
    assert (f["guard_restarted_at_ms"], f["readiness_withdrawn_ms"]) == (None, None)
    assert sb.index("ip -j route show") < sb.index("task kill")


def test_kill_proxy_refuses_a_node_that_announces_nothing_before_the_kill(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy")
    sb.rules(ONE_PROXY, *PLANES, *E1_ONLY)
    failed(sb.run("inject"), "is not routed via e2")
    assert sb.index("task kill") == -1


GUARD_LOGS = r"logs p-1 -c route-guard$"


def guard_started(at: str, withdrawn: str) -> Rule:
    """The restarted route guard's log: its start line at `at`."""
    return Rule(GUARD_LOGS, f"{at}  INFO route_guard: route guard started peers=10.0.0.1 withdrawn={withdrawn} reading=Fit\n")


def test_kill_proxy_fails_while_the_node_stays_announced(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy", guard_window_s="1", withdraw_timeout_s="2")
    sb.rules(ONE_PROXY, *PLANES, *BOTH, *crash_rules("p-1", "e2", [("proxy", "c1")]),
             guard_started("2020-01-01T00:00:00.000000Z", "false"))
    failed(sb.run("inject"), "its route guard did not restart onto a fit proxy within 1900 ms")


def test_a_guard_restarted_onto_a_fit_proxy_within_its_dead_bound_is_recorded_apart(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy", guard_window_s="1", withdraw_timeout_s="2", dead_ms="3000000000000")
    sb.rules(ONE_PROXY, *PLANES, *BOTH, *crash_rules("p-1", "e2", [("proxy", "c1")]),
             guard_started("2099-01-01T00:00:00.250000Z", "false"))
    f = facts(sb.run("inject"))
    assert (f["withdrawn_ms"], f["readiness_withdrawn_ms"]) == (None, None)
    assert f["guard_restarted_at_ms"] == 4070908800250


def test_a_guard_restarted_past_its_dead_bound_but_inside_bfd_detection_is_recorded_apart(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy", guard_window_s="1", withdraw_timeout_s="2", dead_ms="0",
                 bfd_detect_ms="3000000000000")
    sb.rules(ONE_PROXY, *PLANES, *BOTH, *crash_rules("p-1", "e2", [("proxy", "c1")]),
             guard_started("2099-01-01T00:00:00.250000Z", "false"))
    f = facts(sb.run("inject"))
    assert (f["withdrawn_ms"], f["guard_restarted_at_ms"]) == (None, 4070908800250)


def test_a_guard_restarted_onto_an_unfit_proxy_is_no_fit_restart(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy", guard_window_s="1", withdraw_timeout_s="2", dead_ms="3000000000000")
    sb.rules(ONE_PROXY, *PLANES, *BOTH, *crash_rules("p-1", "e2", [("proxy", "c1")]),
             guard_started("2099-01-01T00:00:00.250000Z", "true"))
    failed(sb.run("inject"), "did not restart onto a fit proxy")


def test_kill_proxy_probe_waits_for_both_nodes_in_the_routes(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "kill_proxy")
    sb.state("crashed", "p-1 u1 e2 proxy c1 0\n")
    sb.state("live_before", "2\n")
    pod = {"metadata": {"uid": "u1"}, "status": {
        "conditions": [{"type": "Ready", "status": "True", "lastTransitionTime": "2026-10-15T22:29:20Z"}],
        "containerStatuses": [{"name": "proxy", "restartCount": 1, "state": {"running": {"startedAt": "2026-10-15T22:29:12Z"}}}]}}
    crash_probe = [Rule(r"get pod p-1 --ignore-not-found -o json$", json.dumps(pod)),
                   Rule(r"logs p-1 -c proxy --previous --timestamps --tail=1$", ""),
                   Rule(r"logs p-1 -c proxy --timestamps --limit-bytes=8192$", "")]
    sb.rules(*crash_probe, PROXIES, *PLANES, *E1_ONLY)
    assert sb.run("probe").returncode == 75
    sb.rules(*crash_probe, PROXIES, *PLANES, *BOTH)
    f = facts(sb.run("probe"))
    assert f["next_hops"] == {IVIP: ["172.20.0.3", "172.20.0.4"], EVIP: ["192.168.60.11", "192.168.60.12"]}


def test_cut_external_plane_disconnects_one_proxy_node_and_times_both_withdrawals(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.rules(ONE_PROXY, *PLANES, Rule(r"docker network disconnect sipext e2$"), *E1_ONLY)
    f = facts(sb.run("inject"))
    assert sb.index("docker network disconnect sipext e2") < sb.index("ip -j route show")
    assert (sb.event_dir / "cut").read_text().split() == ["e2", "192.168.60.12"]
    assert (f["node"], f["address"]) == ("e2", "192.168.60.12")
    assert withdrawn(f) == {f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}


def test_cut_external_plane_fails_while_the_internal_vip_stays_announced(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane", withdraw_timeout_s="1")
    sb.rules(ONE_PROXY, *PLANES, Rule(r"docker network disconnect sipext e2$"),
             route_rule(IVIP, "172.20.0.3", "172.20.0.4"), route_rule(EVIP, "192.168.60.11"))
    failed(sb.run("inject"), f"{IVIP} via 172.20.0.4")


def test_cut_heal_reconnects_at_the_kept_address_only_when_disconnected(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.state("cut", "e2 192.168.60.12\n")
    sb.rules(Rule(r"docker inspect", "\n", once=True), Rule(r"docker network connect --ip 192.168.60.12 --driver-opt com\.docker\.network\.endpoint\.ifname=eth1 sipext e2$"),
             Rule(r"docker inspect", "192.168.60.12\n"))
    facts(sb.run("heal"))
    assert sb.index("docker network connect --ip 192.168.60.12 --driver-opt com.docker.network.endpoint.ifname=eth1 sipext e2") >= 0
    assert len(sb.lines("docker inspect")) == 3  # before, after, and a second later
    sb.rules(Rule(r"docker inspect", "192.168.60.12\n"))
    facts(sb.run("heal"))
    assert sb.index("network connect") == -1


def test_cut_probe_waits_for_the_reconnected_node_in_the_routes(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.state("cut", "e2 192.168.60.12\n")
    link = Rule(r"^docker exec e2 ip link show eth1$")
    sb.rules(link, PROXIES, *PLANES, route_rule(IVIP, "172.20.0.3"), route_rule(EVIP, "192.168.60.11", "192.168.60.12"))
    assert sb.run("probe").returncode == 75
    sb.rules(Rule(r"^docker exec e2 ip link show eth1$", rc=1), PROXIES, *PLANES, *BOTH)
    assert sb.run("probe").returncode == 75, "the interface is not back under its name"
    sb.rules(link, PROXIES, *PLANES, *BOTH)
    assert as_obj(facts(sb.run("probe"))["next_hops"])[IVIP] == ["172.20.0.3", "172.20.0.4"]


NEIGHBOUR = Rule(r"docker exec e2 ip -j neigh show to 192\.168\.60\.1 dev eth1$", '[{"dst":"192.168.60.1","lladdr":"02:00:00:00:00:01","state":["REACHABLE"]}]')
SILENCE = Rule(r"docker exec -i e2 nft -f -$")


def test_next_hop_dead_silences_the_hop_below_ip_and_arp_and_waits_for_the_proxy_unfit(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "next_hop_dead")
    fitz = r"docker exec e2 curl -s -m 1 127\.0\.0\.1:9090/fitz$"
    sb.rules(ONE_PROXY, *PLANES, NEIGHBOUR, SILENCE, *E1_ONLY,
             Rule(fitz, "fit", once=True), Rule(fitz, "unfit external-face"))
    f = facts(sb.run("inject"))
    assert sb.index("neigh show") < sb.index("nft -f -") < sb.index("ip -j route show")
    assert withdrawn(f) == {f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}
    assert set(as_obj(f["unfit_ms"])) == {"e2"}
    assert (sb.event_dir / "silenced").read_text().split() == ["e2"]
    assert len(sb.lines("/fitz")) == 2, "the wait reads on until the proxy turns unfit"


def test_next_hop_dead_refuses_a_node_that_holds_no_address_for_the_hop(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "next_hop_dead")
    sb.rules(ONE_PROXY, *PLANES, Rule(r"ip -j neigh show", "[]"), SILENCE, *E1_ONLY)
    failed(sb.run("inject"), "holds no address for 192.168.60.1")
    assert sb.index("nft -f -") == -1


def test_next_hop_dead_fails_while_the_proxy_stays_fit(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "next_hop_dead", withdraw_timeout_s="1")
    sb.rules(ONE_PROXY, *PLANES, NEIGHBOUR, SILENCE, *E1_ONLY, Rule(r"/fitz$", "fit"))
    failed(sb.run("inject"), "still reads fit")


def test_next_hop_dead_heal_lets_the_frames_through_and_is_idempotent(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "next_hop_dead")
    sb.state("silenced", "e2\n")
    sb.rules(Rule(r"nft list table netdev hakit_next_hop_dead$"), Rule(r"nft delete table netdev hakit_next_hop_dead$"))
    facts(sb.run("heal"))
    assert sb.index("nft delete table") >= 0
    sb.rules(Rule(r"nft list table netdev hakit_next_hop_dead$", rc=1))
    facts(sb.run("heal"))
    assert sb.index("nft delete table") == -1


def test_next_hop_dead_probe_waits_for_the_silenced_node_and_every_live_node_in_the_routes(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "next_hop_dead")
    sb.state("silenced", "e2\n")
    only_the_peer_ready = live("app=sip-front-proxy", "p-0 u0 e1 True\np-1 u1 e2 False\n")
    sb.rules(only_the_peer_ready, *PLANES, *E1_ONLY)
    assert sb.run("probe").returncode == 75, "the silenced node's proxy is not Ready"
    sb.rules(PROXIES, *PLANES, *E1_ONLY)
    assert sb.run("probe").returncode == 75, "Ready, not yet routed"
    sb.rules(PROXIES, *PLANES, *BOTH)
    assert as_obj(facts(sb.run("probe"))["next_hops"])[EVIP] == ["192.168.60.11", "192.168.60.12"]


def successor(node: str, name: str) -> Rule:
    """The pods of the proxy selector on `node`: the old one gone, its Ready successor."""
    pods = {"items": [{"metadata": {"name": name}, "status": {"conditions": [
        {"type": "Ready", "status": "True", "lastTransitionTime": "2026-11-03T10:00:20Z"}]}}]}
    return Rule(rf"get pods -l app=sip-front-proxy --field-selector spec.nodeName={node} -o json$", json.dumps(pods))


SUCCESSORS = [successor("e1", "p-2"), successor("e2", "p-3")]


def rollout_routes(vip: str, a: str, b: str) -> list[Rule]:
    """Per call, in the inject's order (p-0 then p-1): p-0 gone, p-1 there;
    p-0 back, p-1 gone; then both for good."""
    return [route_rule(vip, b, once=True), route_rule(vip, b, once=True),
            route_rule(vip, a, once=True), route_rule(vip, a, once=True), route_rule(vip, a, b)]


def test_proxy_rollout_times_each_pods_drain_withdrawal_and_return(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "proxy_rollout")
    drain = "2026-11-03T10:00:00.500000000Z"
    sb.rules(PROXIES, *PLANES,
             Rule(r"rollout restart deployment/sip-front-proxy$"),
             Rule(r"rollout status deployment/sip-front-proxy --timeout=1s$"),
             Rule(r"get pod p-[01] --ignore-not-found -o jsonpath=\{\.metadata\.deletionTimestamp\}$", "2026-11-03T10:00:15Z"),
             Rule(r"logs p-[01] -c sip-front-proxy --timestamps$", f"{drain} INFO draining signal=SIGTERM\n"),
             *SUCCESSORS,
             *rollout_routes(IVIP, "172.20.0.3", "172.20.0.4"),
             *rollout_routes(EVIP, "192.168.60.11", "192.168.60.12"))
    f = facts(sb.run("inject"))
    assert sb.index("rollout restart") < sb.index("ip -j route show")
    rows = {(str(r["pod"]), str(r["vip"])): r for r in objs(f, "routes")}
    assert set(rows) == {(p, v) for p in ("p-0", "p-1") for v in (IVIP, EVIP)}
    for r in rows.values():
        assert r["drain_at_ms"] == 1793700000500
        assert r["ready_at_ms"] == 1793700020000
        assert number(r, "announced_at_ms") >= number(r, "withdrawn_at_ms")
        assert number(r, "ready_to_announced_ms") == number(r, "announced_at_ms") - 1793700020000
    assert f["max_drain_to_withdrawn_ms"] == max(number(r, "drain_to_withdrawn_ms") for r in rows.values())
    assert f["max_ready_to_announced_ms"] == max(number(r, "ready_to_announced_ms") for r in rows.values())


def test_proxy_rollout_fails_when_a_drain_line_is_never_read(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "proxy_rollout")
    sb.rules(PROXIES, *PLANES,
             Rule(r"rollout restart deployment/sip-front-proxy$"),
             Rule(r"rollout status deployment/sip-front-proxy --timeout=1s$"),
             Rule(r"get pod p-[01] --ignore-not-found -o jsonpath=\{\.metadata\.deletionTimestamp\}$", ""),
             *rollout_routes(IVIP, "172.20.0.3", "172.20.0.4"),
             *rollout_routes(EVIP, "192.168.60.11", "192.168.60.12"))
    failed(sb.run("inject"), "p-0 was replaced and its drain line 'draining' was never read")


def test_proxy_rollout_probe_waits_for_the_rollout_and_the_routes(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "proxy_rollout")
    sb.state("live_before", "2\n")
    sb.rules(Rule(r"rollout status deployment/sip-front-proxy --timeout=1s$", rc=1))
    assert sb.run("probe").returncode == 75
    sb.rules(Rule(r"rollout status deployment/sip-front-proxy --timeout=1s$"), PROXIES, *PLANES, *BOTH)
    facts(sb.run("probe"))


SPEAKERS = Rule(r"-n metallb-system get pods -l app.kubernetes.io/component in \(speaker,frr-k8s\) "
                r"--field-selector spec.nodeName=e2 -o jsonpath=.*\.metadata\.uid", "speaker-x s1\nfrr-k8s-y f1\n")


def test_speaker_restart_deletes_the_speakers_of_one_proxy_node(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "speaker_restart")
    sb.rules(ONE_PROXY, *PLANES, SPEAKERS,
             Rule(r"-n metallb-system delete pod (speaker-x|frr-k8s-y) --wait=false$"), *E1_ONLY)
    f = facts(sb.run("inject"))
    assert sb.lines("delete pod") == [
        "kubectl --context kind-lab -n metallb-system delete pod speaker-x --wait=false",
        "kubectl --context kind-lab -n metallb-system delete pod frr-k8s-y --wait=false",
    ]
    assert (f["node"], f["pods"]) == ("e2", ["speaker-x", "frr-k8s-y"])
    assert withdrawn(f) == {f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}


def test_a_speaker_restart_that_withdraws_nothing_is_recorded_not_failed(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "speaker_restart", withdraw_timeout_s="1")
    sb.rules(ONE_PROXY, *PLANES, SPEAKERS, Rule(r"-n metallb-system delete pod"), *BOTH)
    assert facts(sb.run("inject"))["withdrawn_ms"] is None


def test_speaker_restart_probe_waits_for_ready_speakers_and_the_routes(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "speaker_restart")
    sb.state("node", "e2\n")
    sb.state("speakers_before", "2\n")
    rows = r"-n metallb-system get pods -l .* --field-selector spec.nodeName=e2 -o jsonpath=.*Ready"
    sb.rules(Rule(rows, "speaker-z True\nfrr-k8s-w False\n"))
    assert sb.run("probe").returncode == 75
    sb.rules(Rule(rows, "speaker-z True\nfrr-k8s-w True\n"), PROXIES, *PLANES, *BOTH)
    facts(sb.run("probe"))


def test_backend_outage_freezes_every_worker_and_times_every_withdrawal(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "backend_outage")
    sb.rules(live("app=b2bua-worker", "w-0 u0 n1 True\n"), PROXIES, *PLANES,
             *pod_rules("w-0", "n1", "u0", frozen="1"), route_rule(IVIP), route_rule(EVIP))
    f = facts(sb.run("inject"))
    assert f["pods"] == ["w-0"]
    assert withdrawn(f) == {f"{IVIP} 172.20.0.3", f"{EVIP} 192.168.60.11",
                                      f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}


def test_backend_outage_probe_reads_the_proxies_not_the_workers(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "backend_outage")
    sb.rules(PROXIES, *PLANES, *BOTH)
    facts(sb.run("probe"))
    assert sb.index("app=b2bua-worker") == -1


def test_node_stop_of_a_proxy_node_times_its_withdrawal(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "node_stop", internal_vip=IVIP)
    sb.env["HAKIT_TARGET_SELECTOR"] = "app=sip-front-proxy"
    sb.rules(ONE_PROXY, *PLANES,
             Rule(r"get node e2 -o jsonpath=\{\.metadata\.labels\}$", '{"tier": "edge"}'),
             Rule(r"get pods --field-selector spec.nodeName=e2 -o json$", '{"items": [{"metadata": {"name": "p-1"}}]}'),
             Rule(r"^docker stop -t 0 e2$"), *E1_ONLY)
    f = facts(sb.run("inject"))
    assert sb.index("docker stop") < sb.index("ip -j route show")
    assert withdrawn(f) == {f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}


def test_node_stop_without_a_vip_reads_no_route(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "node_stop")
    sb.rules(live("app=rabbitmq", "r-0 u0 i1 True\n"),
             Rule(r"get node i1 -o jsonpath=\{\.metadata\.labels\}$", '{"tier": "infra"}'),
             Rule(r"get pods --field-selector spec.nodeName=i1 -o json$", '{"items": []}'),
             Rule(r"^docker stop -t 0 i1$"))
    assert facts(sb.run("inject"))["withdrawn_ms"] is None
    assert sb.index("ip -j route") == -1


def test_cut_heal_fails_when_the_node_is_not_back_at_its_address(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_external_plane")
    sb.state("cut", "e2 192.168.60.12\n")
    sb.rules(Rule(r"docker inspect", "\n", once=True), Rule(r"docker network connect"), Rule(r"docker inspect", "192.168.60.99\n"))
    failed(sb.run("heal"), "as '192.168.60.99', not 192.168.60.12")


def test_freeze_proxy_freezes_the_proxy_container_and_times_its_nodes_withdrawal(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "freeze_proxy")
    sb.rules(ONE_PROXY, *PLANES,
             Rule(r"docker exec e2 crictl ps --label io\.kubernetes\.pod\.uid=u1 --name \^sip-front-proxy\$ --state running -q$", "c9\n"),
             Rule(r"docker exec e2 crictl inspect --output go-template --template \{\{\.info\.pid\}\} c9$", "222"),
             Rule(r"docker exec e2 cat /proc/222/cgroup$", "0::/kubepods/pod1/c9\n"),
             Rule(r"docker exec e2 sh -c echo 1 > '/sys/fs/cgroup/kubepods/pod1/c9/cgroup\.freeze'$"),
             Rule(r"docker exec e2 cat /sys/fs/cgroup/kubepods/pod1/c9/cgroup\.freeze$", "1"),
             *pod_rules("p-1", "e2", "u1", containers_frozen="0\n1\n", host_network="true"), *E1_ONLY)
    f = facts(sb.run("inject"))
    assert sb.index("c9/cgroup.freeze") < sb.index("ip -j route show")
    assert sb.index("pod1/cgroup.freeze'") == -1, "the pod's other containers run on"
    assert (f["pods"], f["node"]) == (["p-1"], "e2")
    assert withdrawn(f) == {f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}


def test_freeze_proxy_fails_while_the_node_stays_announced(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "freeze_proxy", withdraw_timeout_s="1")
    sb.rules(ONE_PROXY, *PLANES,
             Rule(r"docker exec e2 crictl ps --label", "c9\n"),
             Rule(r"docker exec e2 crictl inspect --output", "222"),
             Rule(r"docker exec e2 cat /proc/222/cgroup$", "0::/kubepods/pod1/c9\n"),
             Rule(r"docker exec e2 sh -c echo 1 > '/sys/fs/cgroup/kubepods/pod1/c9/cgroup\.freeze'$"),
             Rule(r"docker exec e2 cat /sys/fs/cgroup/kubepods/pod1/c9/cgroup\.freeze$", "1"),
             *pod_rules("p-1", "e2", "u1", containers_frozen="0\n1\n", host_network="true"), *BOTH)
    failed(sb.run("inject"), "still announces a VIP 1s after the freeze")


def test_freeze_proxy_heal_thaws_every_container_and_proves_it(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "freeze_proxy")
    sb.state("targets", "p-1 u1 e2\n")
    sb.state("restarts_before", '{"p-1": 0}')
    sb.rules(Rule(r"docker exec e2 sh -c for f in '/sys/fs/cgroup/kubepods/pod1'/\*/cgroup\.freeze"),
             *pod_rules("p-1", "e2", "u1", containers_frozen=["0\n0\n"]))
    f = facts(sb.run("heal"))
    assert len(sb.lines("for f in")) == 2  # the heal and its re-check a second later
    assert f["pods"] == {"p-1": {"state": "healed", "restarts": 0, "terminated_since_applied": None}}


def test_cut_proxy_workers_drops_the_workers_on_one_proxy_node_and_times_its_withdrawal(tmp_path: Path) -> None:
    sb = Sandbox(tmp_path, "cut_proxy_workers")
    dropped = "-A INPUT -s 10.244.1.5/32 -m comment --comment hakit-reject -j DROP\n"
    sb.rules(ONE_PROXY, *PLANES,
             Rule(r"get pods -l app=b2bua-worker -o jsonpath=\{range \.items\[\*\]\}\{\.status\.podIP\}", "10.244.1.5\n10.244.2.7\n"),
             Rule(r"nsenter -t 111 -n -- iptables -I INPUT -s 10\.244\.[12]\.[57]/32 -m comment --comment hakit-reject -j DROP$"),
             *pod_rules("p-1", "e2", "u1", iptables=dropped, host_network="true"), *E1_ONLY)
    f = facts(sb.run("inject"))
    assert len(sb.lines("iptables -I INPUT")) == 2
    assert (f["node"], f["workers"]) == ("e2", ["10.244.1.5", "10.244.2.7"])
    assert withdrawn(f) == {f"{IVIP} 172.20.0.4", f"{EVIP} 192.168.60.12"}
