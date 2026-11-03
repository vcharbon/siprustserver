"""The SUT chart's extension points a product on the external plane uses:
pod settings, ports, annotations and labels, env naming the release."""
import unittest

from charts import SIPRUST, container, env, find, kind_values, pod_spec, render, render_error

KIND = kind_values(SIPRUST)


class PodSettings(unittest.TestCase):
    def test_security_context_sysctls_priority_and_ports(self) -> None:
        for comp, app in (("worker", "b2bua-worker"), ("proxy", "sip-front-proxy")):
            docs = render(SIPRUST, *KIND,
                          "--set", f"{comp}.podSecurityContext.runAsNonRoot=false",
                          "--set", f"{comp}.sysctls[0].name=net.ipv4.ip_unprivileged_port_start",
                          "--set-string", f"{comp}.sysctls[0].value=0",
                          "--set", f"{comp}.priorityClassName=sip-critical",
                          "--set", f"{comp}.extraPorts[0].name=rtp",
                          "--set", f"{comp}.extraPorts[0].containerPort=30000",
                          "--set", f"{comp}.extraPorts[0].protocol=UDP")
            kind = "StatefulSet" if comp == "worker" else "Deployment"
            w = find(docs, kind, app)
            spec = pod_spec(w)
            self.assertEqual(spec["priorityClassName"], "sip-critical", comp)
            self.assertEqual(spec["securityContext"], {
                "runAsNonRoot": False,
                "sysctls": [{"name": "net.ipv4.ip_unprivileged_port_start", "value": "0"}]}, comp)
            ports = container(w, app)["ports"]
            self.assertIn({"name": "rtp", "containerPort": 30000, "protocol": "UDP"}, ports, comp)

    def test_annotations_override_the_charts_without_a_duplicate_key(self) -> None:
        text_args = ["--set-string", "worker.podAnnotations.prometheus\\.io/port=9999",
                     "--set-string", "worker.podAnnotations.team=voice"]
        w = find(render(SIPRUST, *KIND, *text_args), "StatefulSet", "b2bua-worker")
        self.assertEqual(w["spec"]["template"]["metadata"]["annotations"],
                         {"prometheus.io/scrape": "true", "prometheus.io/port": "9999",
                          "team": "voice"})

    def test_a_pod_label_never_replaces_the_selector_label(self) -> None:
        self.assertIn("invalid propertyName 'app'",
                      render_error(SIPRUST, *KIND, "--set", "worker.podLabels.app=x"))
        w = find(render(SIPRUST, *KIND, "--set", "worker.podLabels.tier=voice"),
                 "StatefulSet", "b2bua-worker")
        self.assertEqual(w["spec"]["template"]["metadata"]["labels"],
                         {"app": "b2bua-worker", "tier": "voice"})


class EnvTemplating(unittest.TestCase):
    def test_an_env_value_names_the_release_and_namespace(self) -> None:
        w = find(render(SIPRUST, *KIND, "--set",
                        "worker.env.DECISION_URL=http://{{ .Release.Name }}-decisions.{{ .Release.Namespace }}:8081"),
                 "StatefulSet", "b2bua-worker")
        self.assertEqual(env(container(w, "b2bua-worker"))["DECISION_URL"],
                         "http://rel-decisions.sip-test:8081")

    def test_a_kubelet_reference_passes_through(self) -> None:
        w = find(render(SIPRUST, *KIND), "StatefulSet", "b2bua-worker")
        self.assertEqual(env(container(w, "b2bua-worker"))["B2BUA_ADVERTISE"], "$(POD_IP):5060")


if __name__ == "__main__":
    unittest.main()
