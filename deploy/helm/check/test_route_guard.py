"""The proxy's route guard (ADR-0056 §10): the container beside the proxy, its
privileges, the routers it drops BFD and BGP to, the BGP connect retry that
brings a node back, and the render relations of its detection paths."""
import unittest

from charts import SIPRUST, container, env, find, kind_values, pod_spec, render, render_error

KIND = kind_values(SIPRUST)


def proxy(*args: str) -> dict:
    return find(render(SIPRUST, *KIND, *args), "Deployment", "sip-front-proxy")


class GuardContainer(unittest.TestCase):
    def test_the_guard_runs_beside_the_proxy_with_net_admin_alone(self) -> None:
        deployment = proxy()
        guard = container(deployment, "route-guard")
        self.assertEqual(guard["command"], ["/usr/local/bin/route-guard"])
        self.assertEqual(guard["image"], container(deployment, "sip-front-proxy")["image"])
        self.assertEqual(env(guard), {
            "ROUTE_GUARD_FITNESS": "127.0.0.1:9090",
            "ROUTE_GUARD_PEERS": "172.20.0.1,192.168.60.1",
            "ROUTE_GUARD_POLL_MS": "100",
            "ROUTE_GUARD_DEAD_MS": "1000",
            "ROUTE_GUARD_HOLD_MS": "3000",
            "ROUTE_GUARD_METRICS": "0.0.0.0:9095",
        })
        self.assertEqual(guard["securityContext"]["capabilities"],
                         {"drop": ["ALL"], "add": ["NET_ADMIN"]})
        self.assertIs(guard["securityContext"]["allowPrivilegeEscalation"], False)
        self.assertEqual(guard["livenessProbe"]["httpGet"], {"path": "/healthz", "port": 9095})
        ready = guard["readinessProbe"]
        self.assertEqual((ready["httpGet"], ready["periodSeconds"], ready["failureThreshold"]),
                         ({"path": "/readyz", "port": 9095}, 1, 2))
        self.assertNotIn("securityContext", container(deployment, "sip-front-proxy"))
        annotations = deployment["spec"]["template"]["metadata"]["annotations"]
        self.assertEqual(annotations["prometheus.io/sidecar-port"], "9095")

    def test_the_pod_starts_withdrawn_and_only_a_pod_without_guard_clears_the_table(self) -> None:
        hold, loopback = pod_spec(proxy())["initContainers"][:2]
        self.assertEqual((hold["name"], hold["command"]),
                         ("route-guard-hold", ["/usr/local/bin/route-guard", "--withdraw"]))
        self.assertEqual(env(hold)["ROUTE_GUARD_PEERS"], "172.20.0.1,192.168.60.1")
        self.assertEqual(hold["securityContext"]["capabilities"]["add"], ["NET_ADMIN"])
        self.assertEqual(env(loopback)["ROUTE_GUARD"], "on")
        self.assertIn('[ "$ROUTE_GUARD" = off ]', loopback["args"][0])
        self.assertIn("nft delete table inet route_guard", loopback["args"][0])
        off = pod_spec(proxy("--set", "proxy.routeGuard.enabled=false"))["initContainers"]
        self.assertEqual([c["name"] for c in off], ["vip-loopback"])
        self.assertEqual(env(off[0])["ROUTE_GUARD"], "off")

    def test_named_peers_replace_metallbs_and_off_renders_no_guard(self) -> None:
        guard = container(proxy("--set", "proxy.routeGuard.peers={10.1.1.1,10.1.1.2}"),
                          "route-guard")
        self.assertEqual(env(guard)["ROUTE_GUARD_PEERS"], "10.1.1.1,10.1.1.2")
        off = proxy("--set", "proxy.routeGuard.enabled=false")
        self.assertEqual([c["name"] for c in pod_spec(off)["containers"]], ["sip-front-proxy"])
        self.assertNotIn("prometheus.io/sidecar-port",
                         off["spec"]["template"]["metadata"]["annotations"])

    def test_a_guard_with_no_router_known_is_refused(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.announce.metallb.enabled=false")
        self.assertIn("proxy.routeGuard.peers is required", err)

    def test_each_peer_retries_its_connection_every_connect_time(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "proxy.announce.metallb.connectTimeSeconds=2")
        for peer in ("tor-internal", "tor-external"):
            spec = find(docs, "BGPPeer", f"sip-test-sip-front-proxy-{peer}")["spec"]
            self.assertEqual(spec["connectTime"], "2s")


class GuardRelations(unittest.TestCase):
    """The mechanism of the render check over the guard's paths: each names
    every value it adds up."""

    def test_a_detection_path_past_the_bound_is_refused(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set-string", "proxy.env.HEALTH_ISOLATION_MS=2301")
        for name in ("HEALTH_ISOLATION_MS=2301", "pollMs=100", "detectMultiplier=3",
                     "=300 ms", "withdrawWithinMs=3400", "cut off every worker"):
            self.assertIn(name, err)
        render(SIPRUST, *KIND, "--set-string", "proxy.env.HEALTH_ISOLATION_MS=2300")
        err = render_error(SIPRUST, *KIND, "--set", "proxy.routeGuard.deadMs=2600")
        self.assertIn("proxy.routeGuard.deadMs=2600", err)
        self.assertIn("a hung or dead proxy", err)

    def test_a_lost_link_counts_two_face_readings(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set-string", "proxy.env.PROXY_EXT_FACE_CHECK_MS=1151")
        for name in ("2 × proxy PROXY_EXT_FACE_CHECK_MS=1151", "withdrawWithinMs=3400",
                     "whose external link is lost"):
            self.assertIn(name, err)
        render(SIPRUST, *KIND, "--set-string", "proxy.env.PROXY_EXT_FACE_CHECK_MS=1150")

    def test_a_dead_next_hop_is_read_inside_a_transactions_lifetime(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.neighbour.baseReachableTimeMs=17268")
        for name in ("baseReachableTimeMs=17268", "delayFirstProbeSeconds=1", "ucastSolicit=12",
                     "retransTimeMs=250", "2 × proxy PROXY_EXT_FACE_CHECK_MS=500", "32000 ms"):
            self.assertIn(name, err)
        render(SIPRUST, *KIND, "--set", "proxy.neighbour.baseReachableTimeMs=17267")

    def test_an_arp_retry_fits_half_a_face_reading(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.neighbour.retransTimeMs=251")
        for name in ("retransTimeMs=251", "PROXY_EXT_FACE_CHECK_MS=500", "two readings"):
            self.assertIn(name, err)
        render(SIPRUST, *KIND, "--set", "proxy.neighbour.retransTimeMs=250")

    def test_a_rollout_waits_for_the_let_out_nodes_sessions(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.announce.metallb.connectTimeSeconds=7")
        for name in ("proxy.minReadySeconds=10 s", "connectTimeSeconds=7 s"):
            self.assertIn(name, err)
        render(SIPRUST, *KIND, "--set", "proxy.announce.metallb.connectTimeSeconds=6")

    def test_a_router_that_is_not_ipv4_is_refused(self) -> None:
        err = render_error(SIPRUST, *KIND,
                           "--set", "proxy.announce.metallb.peers.internal[0].address=2001:db8::1")
        self.assertIn("router 2001:db8::1 is not an IPv4 address", err)
        err = render_error(SIPRUST, *KIND, "--set", "proxy.routeGuard.peers={2001:db8::1}")
        self.assertIn("'2001:db8::1' is not valid ipv4", err)

    def test_the_proxy_metrics_port_stays_where_the_guard_reads_it(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set-string", "proxy.env.PROXY_METRICS=0.0.0.0:9191")
        self.assertIn("PROXY_METRICS=0.0.0.0:9191 must keep port 9090", err)

if __name__ == "__main__":
    unittest.main()
