"""Contracts of the siprust chart's render: what each value turns into."""
import json
import re
import subprocess
import unittest

from charts import (SIPRUST, Doc, container, env, find, kind_values, names, pod_spec, render,
                    render_error, workloads)

KIND = kind_values(SIPRUST)
SHELL_VAR = re.compile(r"\$\{?([A-Za-z_][A-Za-z0-9_]*)")
SHELL_ASSIGNED = re.compile(r"\bfor ([A-Za-z_][A-Za-z0-9_]*) in\b|^\s*([A-Za-z_][A-Za-z0-9_]*)=",
                            re.MULTILINE)


def script_of(c: Doc) -> str:
    assert c["command"] == ["/bin/sh", "-c"], c["command"]
    return str(c["args"][0])


# Each pool vip-loopback names in $reasm_pools: its cap and its timeout sysctl.
REASSEMBLY_POOLS = {
    "ipv4": ("ipv4/ipfrag_high_thresh", "ipv4/ipfrag_time"),
    "ipv6": ("ipv6/ip6frag_high_thresh", "ipv6/ip6frag_time"),
    "conntrack6": ("netfilter/nf_conntrack_frag6_high_thresh",
                   "netfilter/nf_conntrack_frag6_timeout"),
}
READ_BACK = "".join(
    f'[ -e /proc/sys/net/{s} ] && echo "{s}=$(cat /proc/sys/net/{s})"\n'
    for pair in REASSEMBLY_POOLS.values() for s in pair
) + 'echo "pools=$(echo $reasm_pools | tr " " ",")"\n'


def reassembly_block(script: str) -> str:
    """vip-loopback's reassembly lines: from reasm_set's definition to its last pool."""
    start = script.index("reasm_set() {")
    end = script.index('reasm_pools="$reasm_pools conntrack6"\nfi\n') + len(
        'reasm_pools="$reasm_pools conntrack6"\nfi\n')
    return script[start:end]


def can_unshare_net() -> bool:
    probe = ["unshare", "--user", "--map-root-user", "--net", "true"]
    try:
        return subprocess.run(probe, capture_output=True).returncode == 0
    except FileNotFoundError:
        return False


def run_in_netns(script: str, timeout_s: str, pool_bytes: str) -> subprocess.CompletedProcess[str]:
    return subprocess.run(
        ["unshare", "--user", "--map-root-user", "--net", "sh", "-ec", script],
        capture_output=True, text=True,
        env={"PATH": "/usr/bin:/bin", "REASM_TIMEOUT_S": timeout_s, "REASM_POOL_BYTES": pool_bytes})


class ShellScripts(unittest.TestCase):
    """The pod scripts reach the node verbatim: every variable they read is set."""

    def check_script(self, workload: Doc, name: str, source: str) -> None:
        c = container(workload, name)
        script = script_of(c)
        self.assertEqual(script.strip(), (SIPRUST / "files" / source).read_text().strip())
        parsed = subprocess.run(["sh", "-n", "-c", script], capture_output=True, text=True)
        self.assertEqual(parsed.returncode, 0, parsed.stderr)
        read = set(SHELL_VAR.findall(script))
        assigned = {a or b for a, b in SHELL_ASSIGNED.findall(script)}
        unset = read - assigned - set(env(c))
        self.assertEqual(unset, set(), f"{name} reads variables nothing sets")
        for var, value in env(c).items():
            self.assertNotEqual(value, "", f"{name} gets an empty {var}")

    def test_vip_loopback_bounds_every_neighbour_queue(self) -> None:
        proxy = find(render(SIPRUST, *KIND), "Deployment", "sip-front-proxy")
        self.check_script(proxy, "vip-loopback", "vip-loopback.sh")
        c = container(proxy, "vip-loopback")
        script = script_of(c)
        self.assertIn("for f in /proc/sys/net/ipv4/neigh/*/unres_qlen_bytes; do", script)
        self.assertIn('echo "$UNRES_QLEN_BYTES" > "$f"', script)
        self.assertEqual(env(c)["UNRES_QLEN_BYTES"], "16384")
        self.assertEqual(env(c)["PROXY_VIP"], "172.20.255.250")

    def test_vip_loopback_owns_the_vips_and_answers_no_port_unreachable_from_them(self) -> None:
        # ADR-0056: the node owns both VIPs on lo for good; until its route is
        # withdrawn a VIP datagram with no socket bound draws no ICMP error.
        proxy = find(render(SIPRUST, *KIND), "Deployment", "sip-front-proxy")
        c = container(proxy, "vip-loopback")
        script = script_of(c)
        self.assertIn('ip addr add "$vip/32" dev lo', script)
        self.assertIn('iptables -I OUTPUT 1 -s "$vip/32" -p icmp --icmp-type port-unreachable -j DROP',
                      script)
        self.assertIn("for vip in $PROXY_VIP $EXTERNAL_VIP; do", script)
        self.assertEqual((env(c)["PROXY_VIP"], env(c)["EXTERNAL_VIP"]),
                         ("172.20.255.250", "192.168.60.250"))

    def test_unres_qlen_bytes_follows_its_value(self) -> None:
        proxy = find(render(SIPRUST, *KIND, "--set", "proxy.unresQlenBytes=8192"),
                     "Deployment", "sip-front-proxy")
        self.assertEqual(env(container(proxy, "vip-loopback"))["UNRES_QLEN_BYTES"], "8192")

    def test_vip_loopback_tightens_every_neighbour_table_and_steers_multipath_off_a_dead_hop(self) -> None:
        proxy = find(render(SIPRUST, *KIND), "Deployment", "sip-front-proxy")
        self.check_script(proxy, "vip-loopback", "vip-loopback.sh")
        c = container(proxy, "vip-loopback")
        script = script_of(c)
        for sysctl, var in (("base_reachable_time_ms", "NEIGH_BASE_REACHABLE_TIME_MS"),
                            ("delay_first_probe_time", "NEIGH_DELAY_FIRST_PROBE_S"),
                            ("ucast_solicit", "NEIGH_UCAST_SOLICIT"),
                            ("mcast_solicit", "NEIGH_MCAST_SOLICIT"),
                            ("retrans_time_ms", "NEIGH_RETRANS_TIME_MS")):
            self.assertIn(f'neigh_set {sysctl} "${var}"', script)
        self.assertIn("echo 1 > /proc/sys/net/ipv4/fib_multipath_use_neigh", script)
        self.assertEqual(
            {v: env(c)[v] for v in ("NEIGH_BASE_REACHABLE_TIME_MS", "NEIGH_DELAY_FIRST_PROBE_S",
                                    "NEIGH_UCAST_SOLICIT", "NEIGH_MCAST_SOLICIT",
                                    "NEIGH_RETRANS_TIME_MS")},
            {"NEIGH_BASE_REACHABLE_TIME_MS": "4000", "NEIGH_DELAY_FIRST_PROBE_S": "1",
             "NEIGH_UCAST_SOLICIT": "12", "NEIGH_MCAST_SOLICIT": "12",
             "NEIGH_RETRANS_TIME_MS": "250"})

    def test_vip_loopback_bounds_ip_reassembly(self) -> None:
        # ADR-0027 X5
        proxy = find(render(SIPRUST, *KIND), "Deployment", "sip-front-proxy")
        c = container(proxy, "vip-loopback")
        self.assertEqual((env(c)["REASM_TIMEOUT_S"], env(c)["REASM_POOL_BYTES"]), ("2", "67108864"))
        block = reassembly_block(script_of(c))
        self.assertIn("reasm_set /proc/sys/net/ipv4/ipfrag_high_thresh", block)
        if not can_unshare_net():
            self.skipTest("no unprivileged network namespace on this host")
        done = run_in_netns(block + READ_BACK, "2", "67108864")
        self.assertEqual(done.returncode, 0, done.stderr)
        read = dict(line.split("=", 1) for line in done.stdout.split())
        self.assertIn("ipv4", read["pools"].split(","))
        for pool in read["pools"].split(","):
            pool_sysctl, timeout_sysctl = REASSEMBLY_POOLS[pool]
            self.assertEqual((read[pool_sysctl], read[timeout_sysctl]), ("67108864", "2"), pool)

    def test_a_pool_under_the_nodes_low_thresh_fails_the_init_container_by_name(self) -> None:
        if not can_unshare_net():
            self.skipTest("no unprivileged network namespace on this host")
        proxy = find(render(SIPRUST, *KIND), "Deployment", "sip-front-proxy")
        block = reassembly_block(script_of(container(proxy, "vip-loopback")))
        tuned = ("echo 33554432 > /proc/sys/net/ipv4/ipfrag_high_thresh\n"
                 "echo 16777216 > /proc/sys/net/ipv4/ipfrag_low_thresh\n")
        done = run_in_netns(tuned + block, "2", "8388608")
        self.assertEqual(done.returncode, 1)
        self.assertIn("/proc/sys/net/ipv4/ipfrag_high_thresh refused 8388608", done.stderr)

    def test_reassembly_follows_its_values_and_never_shrinks_the_pool(self) -> None:
        proxy = find(render(SIPRUST, *KIND, "--set", "proxy.reassembly.timeoutSeconds=5",
                            "--set", "proxy.reassembly.poolBytes=4194304"),
                     "Deployment", "sip-front-proxy")
        e = env(container(proxy, "vip-loopback"))
        self.assertEqual((e["REASM_TIMEOUT_S"], e["REASM_POOL_BYTES"]), ("5", "4194304"))
        self.assertIn("poolBytes", render_error(SIPRUST, *KIND, "--set",
                                                "proxy.reassembly.poolBytes=4194303"))
        self.assertIn("timeoutSeconds", render_error(SIPRUST, *KIND, "--set",
                                                     "proxy.reassembly.timeoutSeconds=0"))

    def test_masq_exempt_reads_only_what_it_is_given(self) -> None:
        ds = find(render(SIPRUST, *KIND), "DaemonSet", "masq-exempt")
        self.check_script(ds, "masq-exempt", "masq-exempt.sh")
        self.assertEqual(env(container(ds, "masq-exempt"))["EXTERNAL_SUBNET"], "192.168.60.0/25")


class Network(unittest.TestCase):
    def test_the_proxy_requires_its_network(self) -> None:
        self.assertIn("network.proxyVip is required", render_error(SIPRUST))

    def test_no_proxy_needs_no_network_and_sends_b_legs_direct(self) -> None:
        docs = render(SIPRUST, "--set", "proxy.enabled=false")
        self.assertNotIn(("Deployment", "sip-front-proxy"), names(docs))
        self.assertNotIn(("Secret", "sip-front-proxy-hmac"), names(docs))
        self.assertNotIn(("DaemonSet", "masq-exempt"), names(docs))
        e = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertEqual(e["B2BUA_MODE"], "standalone")
        self.assertNotIn("B2BUA_OUTBOUND_PROXY", e)
        self.assertNotIn("B2BUA_CONTACT_ADDRESS", e)

    def test_workers_reach_the_world_through_the_internal_vip(self) -> None:
        e = env(container(find(render(SIPRUST, *KIND), "StatefulSet", "b2bua-worker"),
                          "b2bua-worker"))
        self.assertEqual(e["B2BUA_MODE"], "proxied")
        self.assertEqual(e["B2BUA_OUTBOUND_PROXY"], "172.20.255.250:5060")

    def test_every_contact_names_the_external_vip(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "network.sipPort=5070")
        e = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertEqual(e["B2BUA_CONTACT_ADDRESS"], "192.168.60.250:5070")

    def test_the_proxy_listens_and_advertises_both_vips(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "network.sipPort=5070")
        e = env(container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy"))
        self.assertEqual(e["PROXY_LISTEN"], "172.20.255.250:5070")
        self.assertEqual(e["PROXY_ADVERTISE_EXT"], "192.168.60.250:5070")
        self.assertEqual((e["PROXY_EXT_INTERFACE"], e["PROXY_EXT_NEXT_HOPS"]),
                         ("eth1", "192.168.60.1"))

    def test_an_ipv6_vip_is_a_bracketed_hostport_wherever_an_address_names_it(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "network.proxyVip=fd00::250",
                      "--set", "network.externalVip=2001:db8::250")
        proxy = env(container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy"))
        self.assertEqual((proxy["PROXY_LISTEN"], proxy["PROXY_ADVERTISE"]),
                         ("[fd00::250]:5060", "[fd00::250]:5060"))
        self.assertEqual((proxy["PROXY_LISTEN_EXT"], proxy["PROXY_ADVERTISE_EXT"]),
                         ("[2001:db8::250]:5060", "[2001:db8::250]:5060"))
        worker = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertEqual((worker["B2BUA_OUTBOUND_PROXY"], worker["B2BUA_CONTACT_ADDRESS"]),
                         ("[fd00::250]:5060", "[2001:db8::250]:5060"))

    def test_a_bracketed_vip_fails_the_render_naming_the_bare_address(self) -> None:
        for key in ("proxyVip", "externalVip"):
            err = render_error(SIPRUST, *KIND, "--set-string", f"network.{key}=[2001:db8::250]")
            self.assertIn(f"network.{key}=[2001:db8::250] is bracketed: state the bare address, "
                          "2001:db8::250", err)

    def test_the_announcement_follows_readyz_and_the_liveness_probe_healthz(self) -> None:
        # ADR-0056: each VIP is announced from the nodes whose proxy is Ready,
        # through a LoadBalancer Service that carries no SIP port; the
        # livenessProbe never reads the worker view (ADR-0050 §11).
        docs = render(SIPRUST, *KIND)
        for face, vip in (("internal", "172.20.255.250"), ("external", "192.168.60.250")):
            svc = find(docs, "Service", f"sip-front-proxy-{face}")
            self.assertEqual(svc["metadata"]["annotations"]["metallb.io/loadBalancerIPs"], vip)
            spec = svc["spec"]
            self.assertEqual((spec["type"], spec["externalTrafficPolicy"],
                              spec["allocateLoadBalancerNodePorts"]),
                             ("LoadBalancer", "Local", False))
            self.assertEqual(spec["selector"], {"app": "sip-front-proxy"})
            self.assertEqual(spec["ports"], [{"name": "announce", "port": 9, "targetPort": 9,
                                              "protocol": "TCP"}])
        proxy = find(docs, "Deployment", "sip-front-proxy")
        c = container(proxy, "sip-front-proxy")
        self.assertEqual(c["livenessProbe"]["httpGet"]["path"], "/healthz")
        ready = c["readinessProbe"]
        self.assertEqual((ready["httpGet"]["path"], ready["periodSeconds"],
                          ready["failureThreshold"], ready["successThreshold"]),
                         ("/readyz", 1, 2, 1))
        self.assertEqual(proxy["spec"]["minReadySeconds"], 10)
        self.assertEqual([c["name"] for c in pod_spec(proxy)["containers"]],
                         ["sip-front-proxy", "route-guard", "egress-routes"])

    def test_no_worker_shares_a_proxy_node(self) -> None:
        proxy = find(render(SIPRUST, *KIND), "Deployment", "sip-front-proxy")
        terms = pod_spec(proxy)["affinity"]["podAntiAffinity"][
            "requiredDuringSchedulingIgnoredDuringExecution"]
        self.assertEqual([(t["labelSelector"]["matchLabels"]["app"], t["topologyKey"])
                          for t in terms],
                         [("sip-front-proxy", "kubernetes.io/hostname"),
                          ("b2bua-worker", "kubernetes.io/hostname")])


class ProxyKnobs(unittest.TestCase):
    def proxy_env(self, *args: str) -> dict[str, str]:
        proxy = find(render(SIPRUST, *KIND, *args), "Deployment", "sip-front-proxy")
        return env(container(proxy, "sip-front-proxy"))

    def test_unset_knobs_keep_the_runner_defaults(self) -> None:
        e = self.proxy_env()
        for name in ("PROXY_RECV_SHARDS", "PROXY_SELF_GATE_CPS_RATE", "PROXY_SELF_GATE_CPS_SIZE"):
            self.assertNotIn(name, e)

    def test_shards_and_self_gate_become_env(self) -> None:
        e = self.proxy_env("--set", "proxy.recvShards=4", "--set", "proxy.selfGate.cpsRate=1000",
                           "--set", "proxy.selfGate.cpsSize=500")
        self.assertEqual((e["PROXY_RECV_SHARDS"], e["PROXY_SELF_GATE_CPS_RATE"],
                          e["PROXY_SELF_GATE_CPS_SIZE"]), ("4", "1000", "500"))

    def test_a_shard_count_out_of_range_is_refused(self) -> None:
        self.assertIn("recvShards", render_error(SIPRUST, *KIND, "--set", "proxy.recvShards=0"))


SIZING = {
    "sizing.enabled": "true", "inviteTxnTimeoutSec": "158",
    "sizing.profile.cps": "100", "sizing.profile.holdSeconds": "120",
    "sizing.profile.ringSeconds": "20", "sizing.profile.invitesPerCall": "2.5",
    "sizing.callMarginPct": "10", "sizing.emergencyPct": "5", "sizing.txnMarginPct": "15",
    "sizing.gateMarginPct": "25", "sizing.txnSecondsPerCall": "100",
    "sizing.memory.baseBytes": str(200 << 20), "sizing.memory.answeredCallBytes": "40000",
    "sizing.memory.ringingCallBytes": "60000", "sizing.memory.replicaBytes": "10000",
    "sizing.memory.txnBytes": "5000", "sizing.memory.outboundConnections": "256",
    "sizing.memory.connectionBytes": "40960", "sizing.memory.slackPct": "10",
    "sizing.memory.proxyBaseBytes": str(64 << 20), "sizing.memory.memoEntryOverheadBytes": "128",
}


def sizing_args(**over: str) -> list[str]:
    values = {**SIZING, **over}
    return [a for k, v in values.items() for a in ("--set-json", f"{k}={v}")]


class Sizing(unittest.TestCase):
    """templates/_sizing.tpl: the capacity values computed from a profile."""

    def test_a_profile_computes_every_capacity_value(self) -> None:
        docs = render(SIPRUST, *KIND, *sizing_args())
        worker = container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker")
        proxy = container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy")
        w, p = env(worker), env(proxy)
        mib, up = 1 << 20, lambda x, n: -(-x // n) * n
        # 100 calls/s × 120 s over 2 workers = 6 000, + 10 % = 6 600 → 7 000.
        calls = up(6_000 * 110 // 100, 1_000)
        calls_e = up(calls * 105 // 100, 1_000)
        call = 40_000 + 20_000 * 20 // 120
        per_cps = 100 + 2 * 20
        peak = max(100 * per_cps, calls * per_cps // 120 + calls * 64 // 300)
        txns = up(peak * 115 // 100, 1_000)
        txns_e = up(txns * 105 // 100, 1_000)
        rss_e = -(-((200 << 20) + calls_e * call + calls_e * 10_000 + txns_e * 5_000) // mib)
        held = 2 * calls_e
        reserve = held * 2 + held * 64 // 300 + 100 * 32
        full = (200 << 20) + held * call + (txns_e + reserve) * 5_000 + 256 * 40_960
        grow = rss_e * mib + calls_e * (call - 10_000) + reserve * 5_000
        memory = -(-(max(full, grow) * 100 // 90) // mib)
        self.assertEqual(
            (w["B2BUA_MAX_CALLS"], w["B2BUA_MAX_CALLS_EMERGENCY"], w["B2BUA_REPL_BACKUP_MAX_CALLS"]),
            (str(calls), str(calls_e), str(calls_e)))
        self.assertEqual((w["B2BUA_MAX_TXNS"], w["B2BUA_MAX_TXNS_EMERGENCY"]), (str(txns), str(txns_e)))
        self.assertEqual(w["B2BUA_MAX_RSS_EMERGENCY"], f"{rss_e}Mi")
        self.assertEqual(worker["resources"]["limits"]["memory"], f"{memory}Mi")
        # A call of 2.5 INVITEs at a 20 s ring and a 190 s cap: 1.5 × 190 + 20
        # in the INVITE pool, 288 + 64 + 0.5 × 96 in the retransmission pool.
        seconds = (1.5 * 190_000 + 20_000 + 288_000 + 64_000 + 48_000) // 1_000
        entries = up(-(-(125 * int(seconds) * 5) // 4), 16)
        self.assertEqual((p["PROXY_SELF_GATE_CPS_RATE"], p["PROXY_BRANCH_MEMO_MAX_ENTRIES"],
                          p["PROXY_BRANCH_MEMO_MAX_BYTES"], p["PROXY_BRANCH_MEMO_INVITES_PER_CALL"]),
                         ("125", str(entries), str(entries * 2_560), "2.5"))
        need = entries * 2_560 + entries * 128 + (64 << 20)
        self.assertEqual(proxy["resources"]["limits"]["memory"], f"{-(-(need * 100 // 90) // mib)}Mi")

    def test_a_value_sizing_computes_stated_again_fails_the_render(self) -> None:
        err = render_error(SIPRUST, *KIND, *sizing_args(), "--set-string", "worker.env.B2BUA_MAX_TXNS=1")
        self.assertIn("worker.env.B2BUA_MAX_TXNS is computed by sizing", err)


class ClusterKey(unittest.TestCase):
    """The workers' shared key comes from a Secret the chart never draws (ADR-0055 §12)."""

    @staticmethod
    def key_ref(docs: list[Doc]) -> list[Doc]:
        w = container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker")
        return [e["valueFrom"]["secretKeyRef"] for e in w.get("env", [])
                if e["name"] == "B2BUA_CLUSTER_KEY"]

    def test_replication_reads_the_key_from_the_named_secret(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "worker.clusterKey.existingSecret=lab-key")
        self.assertEqual(self.key_ref(docs), [{"name": "lab-key", "key": "key"}])
        self.assertFalse([d for d in docs if d["kind"] == "Secret"
                          and "cluster-key" in d["metadata"]["name"]])

    def test_replication_without_a_key_secret_is_refused(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "worker.clusterKey.existingSecret=")
        self.assertIn("worker.clusterKey.existingSecret", err)

    def test_a_worker_without_replication_reads_no_key(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "worker.replication.enabled=false",
                      "--set", "worker.clusterKey.existingSecret=")
        self.assertEqual(self.key_ref(docs), [])


class RenderRelations(unittest.TestCase):
    """templates/checks.yaml: a relation between a pod spec and a runner's
    final env fails the render naming every value involved."""

    def test_a_drain_the_termination_grace_cannot_hold_fails_the_render(self) -> None:
        # Each value alone is valid; together the drain's longest run
        # (8 + 2 x 9 + 3 + 2 s margin = 31 s) passes the 30 s grace.
        args = ["--set-string", "worker.env.B2BUA_DRAIN_GRACE_MS=8000",
                "--set-string", "worker.env.B2BUA_DRAIN_RELEASE_FLUSH_MS=9000"]
        err = render_error(SIPRUST, *KIND, *args)
        for named in ("worker.terminationGracePeriodSeconds=30", "B2BUA_DRAIN_GRACE_MS=8000",
                      "B2BUA_DRAIN_RELEASE_FLUSH_MS=9000", "B2BUA_DRAIN_CDR_FLUSH_MS=3000"):
            self.assertIn(named, err)
        render(SIPRUST, *KIND, *args, "--set", "worker.terminationGracePeriodSeconds=32")


    def test_a_replication_cap_below_the_peers_flows_fails_the_render(self) -> None:
        # 20 workers: 19 peers x 2 flows x a reconnect each = 76 above the default 64.
        err = render_error(SIPRUST, *KIND, "--set", "worker.replicas=20")
        for named in ("B2BUA_REPL_MAX_CONNECTIONS=64", "worker.replicas=20", "= 76"):
            self.assertIn(named, err)
        render(SIPRUST, *KIND, "--set", "worker.replicas=20",
               "--set-string", "worker.env.B2BUA_REPL_MAX_CONNECTIONS=76")


class WorkerNetworkPolicy(unittest.TestCase):
    """templates/worker-network-policy.yaml: SIP from the proxies, replication
    from the workers, the probe port from the monitoring namespace."""

    @staticmethod
    def rules(*args: str) -> dict[str, Doc]:
        policy = find(render(SIPRUST, *args), "NetworkPolicy", "b2bua-worker")
        assert policy["spec"]["podSelector"] == {"matchLabels": {"app": "b2bua-worker"}}
        assert policy["spec"]["policyTypes"] == ["Ingress"]
        return {f"{r['ports'][0]['protocol']}/{r['ports'][0]['port']}": r
                for r in policy["spec"]["ingress"]}

    def test_the_lab_admits_the_proxy_vip_and_nodes_the_workers_and_the_scraper(self) -> None:
        rules = self.rules(*KIND)
        self.assertEqual(set(rules), {"UDP/5060", "TCP/9092", "TCP/9091"})
        self.assertEqual(rules["UDP/5060"]["from"], [{"ipBlock": {"cidr": "172.20.255.250/32"}},
                                                     {"ipBlock": {"cidr": "172.20.0.0/17"}}])
        self.assertEqual(rules["TCP/9092"]["from"],
                         [{"podSelector": {"matchLabels": {"app": "b2bua-worker"}}}])
        self.assertEqual(rules["TCP/9091"]["from"], [{"namespaceSelector": {"matchLabels": {
            "kubernetes.io/metadata.name": "observability"}}}])

    def test_a_standalone_worker_takes_sip_from_anywhere_and_opens_no_replication(self) -> None:
        rules = self.rules("--set", "proxy.enabled=false")
        self.assertEqual(set(rules), {"UDP/5060", "TCP/9091"})
        self.assertNotIn("from", rules["UDP/5060"])

    def test_the_proxy_nodes_are_required_with_the_proxy(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "worker.networkPolicy.proxyNodeCidrs=null")
        self.assertIn("worker.networkPolicy.proxyNodeCidrs is required", err)

    def test_an_ipv6_vip_is_one_address_too(self) -> None:
        rules = self.rules(*KIND, "--set", "network.proxyVip=fd00::250")
        self.assertEqual(rules["UDP/5060"]["from"][0], {"ipBlock": {"cidr": "fd00::250/128"}})

    def test_off_renders_no_policy(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "worker.networkPolicy.enabled=false")
        self.assertNotIn(("NetworkPolicy", "b2bua-worker"), names(docs))

    def test_the_operator_listener_binds_loopback_and_is_no_port(self) -> None:
        w = find(render(SIPRUST, *KIND), "StatefulSet", "b2bua-worker")
        c = container(w, "b2bua-worker")
        self.assertEqual(env(c)["B2BUA_ADMIN"], "127.0.0.1:9094")
        self.assertNotIn(9094, [p["containerPort"] for p in c["ports"]])


def env_order(c: Doc) -> list[str]:
    return [e["name"] for e in c["env"]]


class HttpBinds(unittest.TestCase):
    """Every HTTP listener's bind is a value: the proxy pod's scrape listeners
    on the node's address, a worker's on every address of its pod, the
    operator and debug listeners on loopback, and each scrape host from an
    allowlist with no literal address, a proxy pod's wildcard only with the
    opt-in."""

    def test_the_scrape_listeners_bind_the_pods_address_and_the_rest_loopback(self) -> None:
        docs = render(SIPRUST, *KIND)
        proxy = container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy")
        self.assertEqual((env(proxy)["PROXY_METRICS"], env(proxy)["PROXY_DEBUG"]),
                         ("$(HOST_IP):9090", "127.0.0.1:9097"))
        self.assertIn({"name": "HOST_IP",
                       "valueFrom": {"fieldRef": {"fieldPath": "status.hostIP"}}}, proxy["env"])
        self.assertLess(env_order(proxy).index("HOST_IP"), env_order(proxy).index("PROXY_METRICS"))
        self.assertNotIn(9097, [p["containerPort"] for p in proxy["ports"]])
        worker = container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker")
        self.assertEqual((env(worker)["B2BUA_METRICS"], env(worker)["B2BUA_ADMIN"]),
                         ("0.0.0.0:9091", "127.0.0.1:9094"))

    def test_each_bind_is_a_value(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "httpBinds.allowWildcard=true",
                      "--set", "proxy.metrics.bind=0.0.0.0:9090",
                      "--set", "proxy.debug.bind=127.0.0.1:19097",
                      "--set", "proxy.routeGuard.metricsBind=[::]:9095",
                      "--set", "worker.metrics.bind=$(POD_IP):9091",
                      "--set", "worker.admin.bind=127.0.0.1:19094")
        deployment = find(docs, "Deployment", "sip-front-proxy")
        proxy = env(container(deployment, "sip-front-proxy"))
        self.assertEqual((proxy["PROXY_METRICS"], proxy["PROXY_DEBUG"]),
                         ("0.0.0.0:9090", "127.0.0.1:19097"))
        guard = env(container(deployment, "route-guard"))
        self.assertEqual((guard["ROUTE_GUARD_METRICS"], guard["ROUTE_GUARD_FITNESS"]),
                         ("[::]:9095", "$(HOST_IP):9090"))
        worker = container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker")
        self.assertEqual((env(worker)["B2BUA_METRICS"], env(worker)["B2BUA_ADMIN"]),
                         ("$(POD_IP):9091", "127.0.0.1:19094"))
        self.assertLess(env_order(worker).index("POD_IP"), env_order(worker).index("B2BUA_METRICS"))

    def test_an_empty_loopback_bind_takes_the_charts_default(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "proxy.debug.bind=", "--set", "worker.admin.bind=")
        proxy = env(container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy"))
        self.assertEqual(proxy["PROXY_DEBUG"], "127.0.0.1:9097")
        worker = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertEqual(worker["B2BUA_ADMIN"], "127.0.0.1:9094")

    def test_a_proxy_pod_wildcard_scrape_bind_needs_the_opt_in(self) -> None:
        for value, named in (
                ("proxy.metrics.bind=0.0.0.0:9090", "proxy.metrics.bind=0.0.0.0:9090"),
                ("proxy.metrics.bind=[::]:9090", "proxy.metrics.bind=[::]:9090"),
                ("proxy.env.PROXY_METRICS=[::]:9090", "proxy PROXY_METRICS=[::]:9090"),
                ("proxy.routeGuard.metricsBind=0.0.0.0:9095",
                 "proxy.routeGuard.metricsBind=0.0.0.0:9095")):
            err = render_error(SIPRUST, *KIND, "--set-string", value)
            self.assertIn(f"{named} is a wildcard", err)
            self.assertIn("httpBinds.allowWildcard=true", err)
            render(SIPRUST, *KIND, "--set-string", value, "--set", "httpBinds.allowWildcard=true")

    def test_a_proxy_pod_scrape_host_is_the_nodes_address_variable_or_an_opted_in_wildcard(
            self) -> None:
        for value in ("proxy.metrics.bind=$(HOST_IP):9090",
                      "proxy.routeGuard.metricsBind=$(HOST_IP):9095"):
            render(SIPRUST, *KIND, "--set-string", value)
        for value, named in (
                ("proxy.metrics.bind=10.0.0.5:9090", "proxy.metrics.bind=10.0.0.5:9090"),
                ("proxy.metrics.bind=172.20.255.250:9090",
                 "proxy.metrics.bind=172.20.255.250:9090"),
                ("proxy.metrics.bind=192.168.60.250:9090",
                 "proxy.metrics.bind=192.168.60.250:9090"),
                ("proxy.metrics.bind=127.0.0.1:9090", "proxy.metrics.bind=127.0.0.1:9090"),
                ("proxy.metrics.bind=$(POD_IP):9090", "proxy.metrics.bind=$(POD_IP):9090"),
                ("proxy.metrics.bind=[::0]:9090", "proxy.metrics.bind=[::0]:9090"),
                ("proxy.metrics.bind=0:0:0:0:0:0:0:0:9090",
                 "proxy.metrics.bind=0:0:0:0:0:0:0:0:9090"),
                ("proxy.metrics.bind=[::ffff:192.168.60.250]:9090",
                 "proxy.metrics.bind=[::ffff:192.168.60.250]:9090"),
                ("proxy.metrics.bind=[::ffff:7f00:1]:9090",
                 "proxy.metrics.bind=[::ffff:7f00:1]:9090"),
                ("proxy.metrics.bind=localhost:9090", "proxy.metrics.bind=localhost:9090"),
                ("proxy.env.PROXY_METRICS=:::9090", "proxy PROXY_METRICS=:::9090"),
                ("proxy.routeGuard.metricsBind=10.0.0.5:9095",
                 "proxy.routeGuard.metricsBind=10.0.0.5:9095"),
                ("proxy.routeGuard.metricsBind=[::1]:9095",
                 "proxy.routeGuard.metricsBind=[::1]:9095")):
            for opt_in in ("false", "true"):
                err = render_error(SIPRUST, *KIND, "--set", f"httpBinds.allowWildcard={opt_in}",
                                   "--set-string", value)
                self.assertIn(f"{named} is no host the hostNetwork proxy pod binds", err)

    def test_a_workers_wildcard_scrape_bind_needs_no_opt_in(self) -> None:
        for value in ("worker.metrics.bind=0.0.0.0:9091", "worker.env.B2BUA_METRICS=[::]:9091"):
            docs = render(SIPRUST, *KIND, "--set-string", value)
            worker = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
            self.assertEqual(worker["B2BUA_METRICS"], value.split("=", 1)[1])

    def test_a_worker_scrape_host_is_its_pods_address_variable_or_a_wildcard(self) -> None:
        for value, named in (
                ("worker.metrics.bind=127.0.0.1:9091", "worker.metrics.bind=127.0.0.1:9091"),
                ("worker.metrics.bind=0:0:0:0:0:0:0:1:9091",
                 "worker.metrics.bind=0:0:0:0:0:0:0:1:9091"),
                ("worker.metrics.bind=[::1]:9091", "worker.metrics.bind=[::1]:9091"),
                ("worker.metrics.bind=[::ffff:7f00:1]:9091",
                 "worker.metrics.bind=[::ffff:7f00:1]:9091"),
                ("worker.metrics.bind=localhost:9091", "worker.metrics.bind=localhost:9091"),
                ("worker.metrics.bind=ip6-loopback:9091", "worker.metrics.bind=ip6-loopback:9091"),
                ("worker.metrics.bind=10.0.0.7:9091", "worker.metrics.bind=10.0.0.7:9091"),
                ("worker.metrics.bind=[::0]:9091", "worker.metrics.bind=[::0]:9091"),
                ("worker.metrics.bind=$(HOST_IP):9091", "worker.metrics.bind=$(HOST_IP):9091"),
                ("worker.env.B2BUA_METRICS=127.0.0.1:9091",
                 "worker B2BUA_METRICS=127.0.0.1:9091")):
            err = render_error(SIPRUST, *KIND, "--set-string", value)
            self.assertIn(f"{named} is no host a worker binds", err)
        templated = '{{ printf "127.0.0.1" }}:9091'
        err = render_error(SIPRUST, *KIND,
                           "--set-json", f"worker.env.B2BUA_METRICS={json.dumps(templated)}")
        self.assertIn(f"worker B2BUA_METRICS={templated} is no host a worker binds", err)

    def test_extra_env_never_restates_a_listener_variable(self) -> None:
        for component, name in (("proxy", "PROXY_METRICS"), ("proxy", "HOST_IP"),
                                ("worker", "B2BUA_METRICS"), ("worker", "POD_IP")):
            err = render_error(SIPRUST, *KIND, "--set", f"{component}.extraEnv[0].name={name}",
                               "--set", f"{component}.extraEnv[0].value=0.0.0.0:9090")
            self.assertIn(f"{component}.extraEnv names {name}, which the chart states", err)
        for component, name in (("proxy", "HOST_IP"), ("worker", "POD_IP")):
            err = render_error(SIPRUST, *KIND, "--set-string", f"{component}.env.{name}=0.0.0.0")
            self.assertIn(f"{component}.env.{name} names an address variable", err)
        docs = render(SIPRUST, *KIND, "--set", "worker.extraEnv[0].name=NODE_NAME",
                      "--set", "worker.extraEnv[0].value=n1")
        worker = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertEqual(worker["NODE_NAME"], "n1")

    def test_the_worker_metrics_port_stays_where_the_probes_read_it(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "worker.metrics.bind=$(POD_IP):9191")
        self.assertIn("worker.metrics.bind=$(POD_IP):9191 must keep port 9091", err)


class Backends(unittest.TestCase):
    def test_producer_and_consumer_declare_the_same_queue(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "cdr.maxLength=5000", "--set", "cdr.queue=records")
        w = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        c = env(container(find(docs, "Deployment", "cdr-consumer"), "cdr-consumer"))
        self.assertEqual((w["B2BUA_CDR_RABBITMQ_URL"], w["B2BUA_CDR_RABBITMQ_QUEUE"],
                          w["B2BUA_CDR_RABBITMQ_MAX_LEN"]),
                         (c["CDR_AMQP_URL"], c["CDR_QUEUE"], c["CDR_QUEUE_MAX_LEN"]))
        self.assertEqual(c["CDR_QUEUE_MAX_LEN"], "5000")
        self.assertIn("@rabbitmq.sip-test.svc.cluster.local:5672/", c["CDR_AMQP_URL"])

    def test_cdr_off_drops_broker_consumer_and_sink(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "cdr.enabled=false")
        self.assertFalse({("Deployment", "rabbitmq"), ("Deployment", "cdr-consumer")} & names(docs))
        w = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertFalse([n for n in w if n.startswith("B2BUA_CDR_")])

    def test_limiter_off_leaves_the_worker_without_its_url(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "callLimiter.enabled=false")
        self.assertNotIn(("Deployment", "call-limiter"), names(docs))
        w = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertNotIn("LIMITER_URL", w)

    def test_a_terminating_limiter_is_gone_within_seconds(self) -> None:
        limiter = find(render(SIPRUST, *KIND), "Deployment", "call-limiter")
        self.assertLessEqual(pod_spec(limiter)["terminationGracePeriodSeconds"], 2)

    def test_each_component_turns_off_alone(self) -> None:
        every = names(render(SIPRUST, *KIND))
        for key, gone in (
                ("worker.enabled", {("StatefulSet", "b2bua-worker"), ("Service", "b2bua-worker"),
                                    ("PodDisruptionBudget", "b2bua-worker"),
                                    ("NetworkPolicy", "b2bua-worker")}),
                ("proxy.enabled", {("Deployment", "sip-front-proxy"), ("Service", "sip-front-proxy"),
                                   ("Service", "sip-front-proxy-internal"),
                                   ("Service", "sip-front-proxy-external"),
                                   ("BFDProfile", "sip-test-sip-front-proxy"),
                                   ("IPAddressPool", "sip-test-sip-front-proxy-internal"),
                                   ("IPAddressPool", "sip-test-sip-front-proxy-external"),
                                   ("BGPAdvertisement", "sip-test-sip-front-proxy-internal"),
                                   ("BGPAdvertisement", "sip-test-sip-front-proxy-external"),
                                   ("BGPPeer", "sip-test-sip-front-proxy-tor-internal"),
                                   ("BGPPeer", "sip-test-sip-front-proxy-tor-external"),
                                   ("Secret", "sip-front-proxy-hmac"),
                                   ("ServiceAccount", "sip-front-proxy"),
                                   ("RoleBinding", "sip-front-proxy-endpointslice-reader"),
                                   ("PodDisruptionBudget", "sip-front-proxy")}),
                ("callLimiter.enabled", {("Deployment", "call-limiter"), ("Service", "call-limiter")}),
                ("cdr.consumer.enabled", {("Deployment", "cdr-consumer"), ("Service", "cdr-consumer")}),
                ("masqExempt.enabled", {("DaemonSet", "masq-exempt")})):
            self.assertEqual(every - names(render(SIPRUST, *KIND, "--set", f"{key}=false")), gone, key)

    def test_services_follow_the_release_namespace(self) -> None:
        docs = render(SIPRUST, *KIND, namespace="lab-b")
        w = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertEqual(w["LIMITER_URL"], "call-limiter.lab-b.svc.cluster.local:8080")
        metallb = {d["metadata"]["name"] for d in docs if d["apiVersion"].startswith("metallb.io/")}
        self.assertTrue(metallb and all(n.startswith("lab-b-") for n in metallb), metallb)
        self.assertEqual({d["metadata"]["namespace"] for d in docs
                          if not d["apiVersion"].startswith("metallb.io/")}, {"lab-b"})
        self.assertEqual({d["metadata"]["namespace"] for d in docs
                          if d["apiVersion"].startswith("metallb.io/")}, {"metallb-system"})


class GenericDefaults(unittest.TestCase):
    """Without the kind values a product inherits the runners' own defaults and
    any node: the chart sets only the env it derives from its values."""

    NETWORK = ["--set", "network.proxyVip=10.0.255.250",
               "--set", "network.externalVip=203.0.113.250", "--set", "network.externalInterface=ens4",
               "--set", "network.externalNextHops={203.0.113.1}",
               "--set", "worker.networkPolicy.proxyNodeCidrs={10.0.0.0/24}",
               "--set", "proxy.hmac.existingSecret=edge-cookie",
               "--set", "proxy.nodeSelector.sip-role=edge",
               "--set", "proxy.routeGuard.peers={10.0.0.1}"]
    DERIVED = {
        "b2bua-worker": {"B2BUA_LISTEN", "B2BUA_ADVERTISE", "B2BUA_ORDINAL", "B2BUA_METRICS",
                         "B2BUA_ADMIN",
                         "B2BUA_REPL", "B2BUA_REPL_LISTEN", "B2BUA_REPL_PORT", "B2BUA_REPL_SERVICE",
                         "B2BUA_MODE", "B2BUA_OUTBOUND_PROXY", "B2BUA_CONTACT_ADDRESS",
                         "LIMITER_URL",
                         "B2BUA_CDR_RABBITMQ_URL", "B2BUA_CDR_RABBITMQ_QUEUE",
                         "B2BUA_CDR_RABBITMQ_MAX_LEN", "OTEL_EXPORTER_OTLP_ENDPOINT",
                         "SIP_TRACE_HEADER"},
        "sip-front-proxy": {"PROXY_LISTEN", "PROXY_ADVERTISE", "PROXY_LISTEN_EXT",
                            "PROXY_ADVERTISE_EXT", "PROXY_METRICS", "PROXY_DEBUG",
                            "PROXY_EXT_INTERFACE", "PROXY_EXT_NEXT_HOPS",
                            "PROXY_WORKER_SERVICE", "PROXY_WORKER_PORT",
                            "OTEL_EXPORTER_OTLP_ENDPOINT", "SIP_TRACE_HEADER"},
        "call-limiter": {"LIMITER_LISTEN"},
        "cdr-consumer": {"CDR_AMQP_URL", "CDR_QUEUE", "CDR_QUEUE_MAX_LEN", "CDR_METRICS_LISTEN",
                         "CDR_PAYLOAD"},
    }

    def test_only_derived_env_and_no_node_placement_but_the_proxy_tier(self) -> None:
        docs = render(SIPRUST, *self.NETWORK)
        for w in workloads(docs):
            spec = pod_spec(w)
            name = w["metadata"]["name"]
            if name == "sip-front-proxy":
                self.assertEqual(spec["nodeSelector"], {"sip-role": "edge"})
            else:
                self.assertNotIn("nodeSelector", spec, name)
            self.assertNotIn("tolerations", spec, name)
            if name in self.DERIVED:
                self.assertEqual(set(env(container(w, name))), self.DERIVED[name], name)

    def test_workers_spread_softly_by_default_and_hard_on_the_lab(self) -> None:
        def spread(*args: str) -> list[str]:
            w = find(render(SIPRUST, *args), "StatefulSet", "b2bua-worker")
            return [c["whenUnsatisfiable"] for c in pod_spec(w)["topologySpreadConstraints"]]
        self.assertEqual(spread(*self.NETWORK), ["ScheduleAnyway"])
        self.assertEqual(spread(*KIND), ["DoNotSchedule"])


class DisruptionBudgets(unittest.TestCase):
    def test_one_worker_and_one_proxy_at_a_time(self) -> None:
        docs = render(SIPRUST, *KIND)
        for app in ("b2bua-worker", "sip-front-proxy"):
            spec = find(docs, "PodDisruptionBudget", app)["spec"]
            self.assertEqual((spec["maxUnavailable"], spec["selector"]), (1, {"matchLabels": {"app": app}}))

    def test_a_budget_follows_its_values(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "proxy.podDisruptionBudget.enabled=false")
        self.assertNotIn(("PodDisruptionBudget", "sip-front-proxy"), names(docs))
        self.assertIn("maxUnavailable': minimum: got 0, want 1",
                      render_error(SIPRUST, *KIND, "--set", "worker.podDisruptionBudget.maxUnavailable=0"))


class Workloads(unittest.TestCase):
    def test_every_container_names_each_env_once(self) -> None:
        for w in workloads(render(SIPRUST, *KIND)):
            spec = pod_spec(w)
            for c in [*spec.get("initContainers", []), *spec["containers"]]:
                listed = [e["name"] for e in c.get("env", [])]
                self.assertEqual(len(listed), len(set(listed)), f"{c['name']}: {listed}")

    def test_pods_carry_the_selector_label_tooling_selects_by(self) -> None:
        for w in workloads(render(SIPRUST, *KIND)):
            app = w["metadata"]["name"]
            self.assertEqual(w["spec"]["selector"]["matchLabels"], {"app": app})
            self.assertEqual(w["spec"]["template"]["metadata"]["labels"]["app"], app)

    def test_replication_follows_its_value(self) -> None:
        def repl(*args: str) -> str:
            w = find(render(SIPRUST, *KIND, *args), "StatefulSet", "b2bua-worker")
            return env(container(w, "b2bua-worker"))["B2BUA_REPL"]
        self.assertEqual((repl(), repl("--set", "worker.replication.enabled=false")), ("1", "0"))

    def test_the_schema_refuses_a_misspelt_key_and_a_number_env(self) -> None:
        self.assertIn("'replica' not allowed",
                      render_error(SIPRUST, *KIND, "--set", "worker.replica=3"))
        self.assertIn("worker/env/B2BUA_KEEPALIVE_SEC",
                      render_error(SIPRUST, *KIND, "--set", "worker.env.B2BUA_KEEPALIVE_SEC=60"))



class FormerProxyNodes(unittest.TestCase):
    """A node that runs or ran a proxy keeps the VIPs on lo: no worker lands
    there (ADR-0056 §7), and the proxy's nodes are always named."""

    @staticmethod
    def terms(*args: str) -> list[Doc]:
        w = find(render(SIPRUST, *KIND, *args), "StatefulSet", "b2bua-worker")
        aff = pod_spec(w)["affinity"]["nodeAffinity"]["requiredDuringSchedulingIgnoredDuringExecution"]
        return list(aff["nodeSelectorTerms"])

    def test_workers_keep_off_every_node_the_proxy_selector_names(self) -> None:
        self.assertEqual(self.terms(), [{"matchExpressions": [
            {"key": "tier", "operator": "NotIn", "values": ["edge"]}]}])

    def test_each_proxy_label_is_its_own_term_anded_into_the_workers_own(self) -> None:
        own = ["--set", "worker.affinity.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution"
               ".nodeSelectorTerms[0].matchExpressions[0].key=disk",
               "--set", "worker.affinity.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution"
               ".nodeSelectorTerms[0].matchExpressions[0].operator=In",
               "--set", "worker.affinity.nodeAffinity.requiredDuringSchedulingIgnoredDuringExecution"
               ".nodeSelectorTerms[0].matchExpressions[0].values[0]=ssd",
               "--set", "proxy.nodeSelector.rack=r1"]
        disk = {"key": "disk", "operator": "In", "values": ["ssd"]}
        self.assertEqual(self.terms(*own), [
            {"matchExpressions": [disk, {"key": "rack", "operator": "NotIn", "values": ["r1"]}]},
            {"matchExpressions": [disk, {"key": "tier", "operator": "NotIn", "values": ["edge"]}]}])

    def test_the_proxy_nodes_must_be_named(self) -> None:
        self.assertIn("proxy.nodeSelector is required",
                      render_error(SIPRUST, *KIND, "--set", "proxy.nodeSelector=null"))

    def test_no_proxy_leaves_the_workers_own_affinity(self) -> None:
        w = find(render(SIPRUST, *KIND, "--set", "proxy.enabled=false"), "StatefulSet", "b2bua-worker")
        self.assertNotIn("affinity", pod_spec(w))


if __name__ == "__main__":
    unittest.main()
