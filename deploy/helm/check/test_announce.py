"""The BGP announcement of the proxy's VIPs through MetalLB (ADR-0056): the
objects the chart renders into MetalLB's namespace, what it refuses, and the
render relations between the readiness probe, the propagation of a
withdrawal, the drain and the rollout."""
import unittest

from charts import SIPRUST, METALLB_APIS, find, kind_values, render, render_error

KIND = kind_values(SIPRUST)
KIND_VALUES = ["-f", str(SIPRUST / "values-kind.yaml")]


class MetallbObjects(unittest.TestCase):
    def test_one_pool_and_one_advertisement_per_face_to_that_faces_routers(self) -> None:
        docs = render(SIPRUST, *KIND)
        for face, vip, peer in (("internal", "172.20.255.250", "tor-internal"),
                                ("external", "192.168.60.250", "tor-external")):
            name = f"sip-test-sip-front-proxy-{face}"
            pool = find(docs, "IPAddressPool", name)
            self.assertEqual(pool["metadata"]["namespace"], "metallb-system")
            self.assertEqual(pool["spec"], {"addresses": [f"{vip}/32"], "autoAssign": False})
            adv = find(docs, "BGPAdvertisement", name)["spec"]
            self.assertEqual(adv, {"ipAddressPools": [name], "aggregationLength": 32,
                                   "peers": [f"sip-test-sip-front-proxy-{peer}"]})

    def test_peers_from_the_proxy_nodes_with_bfd_and_no_graceful_restart(self) -> None:
        docs = render(SIPRUST, *KIND)
        bfd = find(docs, "BFDProfile", "sip-test-sip-front-proxy")["spec"]
        self.assertEqual(bfd, {"receiveInterval": 300, "transmitInterval": 300,
                               "detectMultiplier": 3})
        for peer, address in (("tor-internal", "172.20.0.1"), ("tor-external", "192.168.60.1")):
            spec = find(docs, "BGPPeer", f"sip-test-sip-front-proxy-{peer}")["spec"]
            self.assertEqual((spec["myASN"], spec["peerASN"], spec["peerAddress"]),
                             (64513, 64512, address))
            self.assertEqual(spec["bfdProfile"], "sip-test-sip-front-proxy")
            self.assertIs(spec["enableGracefulRestart"], False)
            self.assertEqual(spec["nodeSelectors"], [{"matchLabels": {"tier": "edge"}}])

    def test_off_leaves_the_objects_to_the_operator_and_keeps_the_services(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "proxy.announce.metallb.enabled=false")
        self.assertFalse([d for d in docs if d["apiVersion"].startswith("metallb.io/")])
        find(docs, "Service", "sip-front-proxy-internal")
        find(docs, "Service", "sip-front-proxy-external")

    def test_a_cluster_without_metallb_is_refused(self) -> None:
        self.assertIn("needs MetalLB's metallb.io/v1beta1 API", render_error(SIPRUST, *KIND_VALUES))
        self.assertIn("metallb.io/v1beta2",
                      render_error(SIPRUST, *KIND_VALUES, "--api-versions", "metallb.io/v1beta1"))

    def test_a_face_with_no_router_is_refused(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.announce.metallb.peers.external=null")
        self.assertIn("proxy.announce.metallb.peers.external is empty", err)

    def test_the_external_next_hops_are_the_external_peers(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "network.externalNextHops={192.168.60.9}")
        self.assertIn("network.externalNextHops=[192.168.60.9] must be the external BGP peers", err)
        self.assertIn("[192.168.60.1]", err)
        render(SIPRUST, *KIND, "--set", "proxy.announce.metallb.enabled=false",
               "--set", "network.externalNextHops={192.168.60.9}")

    def test_a_load_balancer_class_is_passed_on(self) -> None:
        docs = render(SIPRUST, *KIND, "--set", "proxy.announce.loadBalancerClass=metallb")
        self.assertEqual(find(docs, "Service", "sip-front-proxy-internal")["spec"]
                         ["loadBalancerClass"], "metallb")


class RenderRelations(unittest.TestCase):
    """One refusal per relation, each naming every value involved."""

    def test_the_drain_outlasts_the_withdrawal(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.announce.propagationMs=11001")
        for name in ("PROXY_DRAIN_GRACE_MS=13000", "periodSeconds=1", "failureThreshold=2",
                     "propagationMs=11001"):
            self.assertIn(name, err)
        render(SIPRUST, *KIND, "--set", "proxy.announce.propagationMs=10999",
               "--set", "proxy.minReadySeconds=20")

    def test_a_rollout_waits_for_the_announcement(self) -> None:
        err = render_error(SIPRUST, *KIND, "--set", "proxy.minReadySeconds=8")
        self.assertIn("proxy.minReadySeconds=8 s", err)
        self.assertIn("propagationMs=7000", err)

    def test_the_announce_port_is_never_a_listener(self) -> None:
        for port in ("9090", "5060"):
            err = render_error(SIPRUST, *KIND, "--set", f"proxy.announce.port={port}")
            self.assertIn(f"proxy.announce.port={port}", err)

    def test_the_lab_apis_are_what_the_kind_render_states(self) -> None:
        self.assertEqual(METALLB_APIS[1::2], ["metallb.io/v1beta1", "metallb.io/v1beta2"])


if __name__ == "__main__":
    unittest.main()
