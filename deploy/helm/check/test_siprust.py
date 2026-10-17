"""Contracts of the siprust chart's render: what each value turns into."""
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
            if var != "EXTRA_VIPS":
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

    def test_unres_qlen_bytes_follows_its_value(self) -> None:
        proxy = find(render(SIPRUST, *KIND, "--set", "proxy.unresQlenBytes=8192"),
                     "Deployment", "sip-front-proxy")
        self.assertEqual(env(container(proxy, "vip-loopback"))["UNRES_QLEN_BYTES"], "8192")

    def test_masq_exempt_reads_only_what_it_is_given(self) -> None:
        ds = find(render(SIPRUST, *KIND), "DaemonSet", "masq-exempt")
        self.check_script(ds, "masq-exempt", "masq-exempt.sh")
        self.assertEqual(env(container(ds, "masq-exempt"))["EXTERNAL_SUBNET"], "192.168.60.0/24")


class Network(unittest.TestCase):
    def test_the_proxy_requires_its_network(self) -> None:
        self.assertIn("network.proxyVip is required", render_error(SIPRUST))

    def test_no_proxy_needs_no_network_and_sends_b_legs_direct(self) -> None:
        docs = render(SIPRUST, "--set", "proxy.enabled=false")
        self.assertNotIn(("Deployment", "sip-front-proxy"), names(docs))
        self.assertNotIn(("Secret", "sip-front-proxy-hmac"), names(docs))
        self.assertNotIn(("DaemonSet", "masq-exempt"), names(docs))
        e = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertNotIn("B2BUA_OUTBOUND_PROXY", e)
        self.assertNotIn("B2BUA_REQUIRE_OUTBOUND_PROXY", e)

    def test_workers_reach_the_world_through_the_internal_vip(self) -> None:
        e = env(container(find(render(SIPRUST, *KIND), "StatefulSet", "b2bua-worker"),
                          "b2bua-worker"))
        self.assertEqual(e["B2BUA_OUTBOUND_PROXY"], "172.20.255.250:5060")
        self.assertEqual(e["B2BUA_REQUIRE_OUTBOUND_PROXY"], "1")

    def test_the_proxy_listens_and_advertises_both_vips(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "network.sipPort=5070")
        e = env(container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy"))
        self.assertEqual(e["PROXY_LISTEN"], "172.20.255.250:5070")
        self.assertEqual(e["PROXY_ADVERTISE_EXT"], "192.168.60.250:5070")
        self.assertEqual(e["PROXY_FACE_INT_CIDRS"], "10.244.0.0/16,172.20.0.0/16")
        conf = find(docs, "ConfigMap", "sip-front-proxy-keepalived")["data"]["keepalived.conf"]
        self.assertIn("172.20.255.250 dev eth0", conf)
        self.assertIn("192.168.60.250 dev eth1", conf)

    def test_the_vips_follow_vrrp_and_the_liveness_probe_healthz(self) -> None:
        # ADR-0050 §11: the VIP check waits for the first worker view; the
        # livenessProbe never does, so a proxy with no reachable worker is not
        # restarted in a loop.
        docs = render(SIPRUST, *KIND)
        conf = find(docs, "ConfigMap", "sip-front-proxy-keepalived")["data"]["keepalived.conf"]
        self.assertIn("http://127.0.0.1:9090/vrrp ", conf)
        self.assertNotIn("/healthz", conf.split("vrrp_script chk_proxy_health")[1].split("}")[0])
        proxy = container(find(docs, "Deployment", "sip-front-proxy"), "sip-front-proxy")
        self.assertEqual(proxy["livenessProbe"]["httpGet"]["path"], "/healthz")
        self.assertEqual(proxy["readinessProbe"]["httpGet"]["path"], "/readyz")

    def test_a_keepalived_change_rolls_the_proxies(self) -> None:
        def checksum(*args: str) -> str:
            proxy = find(render(SIPRUST, *KIND, *args), "Deployment", "sip-front-proxy")
            return str(proxy["spec"]["template"]["metadata"]["annotations"]["checksum/keepalived"])
        self.assertNotEqual(checksum(), checksum("--set", "proxy.keepalived.virtualRouterId=52"))


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

    def test_each_component_turns_off_alone(self) -> None:
        every = names(render(SIPRUST, *KIND))
        for key, gone in (
                ("worker.enabled", {("StatefulSet", "b2bua-worker"), ("Service", "b2bua-worker")}),
                ("proxy.enabled", {("Deployment", "sip-front-proxy"), ("Service", "sip-front-proxy"),
                                   ("ConfigMap", "sip-front-proxy-keepalived"),
                                   ("Secret", "sip-front-proxy-hmac"),
                                   ("ServiceAccount", "sip-front-proxy"),
                                   ("RoleBinding", "sip-front-proxy-endpointslice-reader")}),
                ("callLimiter.enabled", {("Deployment", "call-limiter"), ("Service", "call-limiter")}),
                ("cdr.consumer.enabled", {("Deployment", "cdr-consumer"), ("Service", "cdr-consumer")}),
                ("masqExempt.enabled", {("DaemonSet", "masq-exempt")})):
            self.assertEqual(every - names(render(SIPRUST, *KIND, "--set", f"{key}=false")), gone, key)

    def test_services_follow_the_release_namespace(self) -> None:
        docs = render(SIPRUST, *KIND, namespace="lab-b")
        w = env(container(find(docs, "StatefulSet", "b2bua-worker"), "b2bua-worker"))
        self.assertEqual(w["LIMITER_URL"], "call-limiter.lab-b.svc.cluster.local:8080")
        self.assertEqual({d["metadata"]["namespace"] for d in docs}, {"lab-b"})


class GenericDefaults(unittest.TestCase):
    """Without the kind values a product inherits the runners' own defaults and
    any node: the chart sets only the env it derives from its values."""

    NETWORK = ["--set", "network.proxyVip=10.0.255.250", "--set", "network.internalInterface=ens3",
               "--set", "network.externalVip=203.0.113.250", "--set", "network.externalInterface=ens4",
               "--set", "network.internalCidrs=10.0.0.0/16"]
    DERIVED = {
        "b2bua-worker": {"B2BUA_LISTEN", "B2BUA_ADVERTISE", "B2BUA_ORDINAL", "B2BUA_METRICS",
                         "B2BUA_REPL", "B2BUA_REPL_LISTEN", "B2BUA_REPL_PORT", "B2BUA_REPL_SERVICE",
                         "B2BUA_OUTBOUND_PROXY", "B2BUA_REQUIRE_OUTBOUND_PROXY", "LIMITER_URL",
                         "B2BUA_CDR_RABBITMQ_URL", "B2BUA_CDR_RABBITMQ_QUEUE",
                         "B2BUA_CDR_RABBITMQ_MAX_LEN", "OTEL_EXPORTER_OTLP_ENDPOINT",
                         "SIP_TRACE_HEADER"},
        "sip-front-proxy": {"PROXY_LISTEN", "PROXY_ADVERTISE", "PROXY_LISTEN_EXT",
                            "PROXY_ADVERTISE_EXT", "PROXY_FACE_INT_CIDRS", "PROXY_METRICS",
                            "PROXY_WORKER_SERVICE", "PROXY_WORKER_PORT",
                            "OTEL_EXPORTER_OTLP_ENDPOINT", "SIP_TRACE_HEADER"},
        "call-limiter": {"LIMITER_LISTEN"},
        "cdr-consumer": {"CDR_AMQP_URL", "CDR_QUEUE", "CDR_QUEUE_MAX_LEN", "CDR_METRICS_LISTEN",
                         "CDR_PAYLOAD"},
    }

    def test_only_derived_env_and_no_node_placement(self) -> None:
        docs = render(SIPRUST, *self.NETWORK)
        for w in workloads(docs):
            spec = pod_spec(w)
            name = w["metadata"]["name"]
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


if __name__ == "__main__":
    unittest.main()
