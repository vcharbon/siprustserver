"""Contracts of the siprust-observability chart's render."""
import unittest

import yaml

from charts import OBSERVABILITY, container, find, kind_values, names, render, render_error

KIND = [*kind_values(OBSERVABILITY), "--set", "clusterName=kind-sip-e2e"]
COLLECTORS = {("Deployment", "vmagent"), ("Deployment", "kube-state-metrics"),
              ("DaemonSet", "node-exporter"), ("DaemonSet", "fluent-bit")}


class Collectors(unittest.TestCase):
    def test_every_collector_is_on_by_default_with_its_key_settings(self) -> None:
        docs = render(OBSERVABILITY, *KIND, namespace="observability")
        self.assertLessEqual(COLLECTORS, names(docs))
        vmagent = container(find(docs, "Deployment", "vmagent"), "vmagent")
        self.assertIn("-remoteWrite.url=http://172.20.0.1:8428/api/v1/write", vmagent["args"])
        self.assertIn("-promscrape.config=/config/vmagent.yaml", vmagent["args"])
        self.assertEqual(find(docs, "Deployment", "vmagent")["spec"]["template"]["spec"]
                         ["serviceAccountName"], "vmagent")
        ksm = find(docs, "Deployment", "kube-state-metrics")
        self.assertEqual(ksm["spec"]["template"]["metadata"]["annotations"],
                         {"prometheus.io/scrape": "true", "prometheus.io/port": "8080"})
        self.assertEqual(container(ksm, "kube-state-metrics")["image"],
                         "registry.k8s.io/kube-state-metrics/kube-state-metrics:v2.13.0")
        node = find(docs, "DaemonSet", "node-exporter")
        spec = node["spec"]["template"]["spec"]
        self.assertTrue(spec["hostNetwork"] and spec["hostPID"])
        self.assertEqual(spec["tolerations"], [{"operator": "Exists"}])
        self.assertEqual(container(node, "node-exporter")["ports"][0]["hostPort"], 9100)
        logs = find(docs, "DaemonSet", "fluent-bit")
        self.assertEqual(logs["spec"]["template"]["spec"]["tolerations"], [{"operator": "Exists"}])
        conf = find(docs, "ConfigMap", "fluent-bit-config")["data"]["fluent-bit.conf"]
        self.assertRegex(conf, r"Host\s+172\.20\.0\.1\n\s+Port\s+9428\n")
        self.assertIn("Path              /var/log/containers/*.log", conf)

    def test_cluster_scoped_objects_carry_the_release_namespace(self) -> None:
        docs = render(OBSERVABILITY, *KIND, namespace="obs-b")
        scoped = sorted(d["metadata"]["name"] for d in docs
                        if d["kind"] in ("ClusterRole", "ClusterRoleBinding"))
        self.assertEqual(scoped, sorted(2 * ["obs-b-vmagent", "obs-b-kube-state-metrics",
                                             "obs-b-fluent-bit"]))
        for binding in (d for d in docs if d["kind"] == "ClusterRoleBinding"):
            self.assertEqual(binding["roleRef"]["name"], binding["metadata"]["name"])

    def test_each_collector_turns_off_alone(self) -> None:
        for key, obj in (("vmagent", ("Deployment", "vmagent")),
                         ("kubeStateMetrics", ("Deployment", "kube-state-metrics")),
                         ("nodeExporter", ("DaemonSet", "node-exporter")),
                         ("logShipping", ("DaemonSet", "fluent-bit"))):
            rendered = names(render(OBSERVABILITY, *KIND, "--set", f"{key}.enabled=false"))
            self.assertEqual(COLLECTORS - rendered, {obj}, key)

    def test_the_host_stack_is_required(self) -> None:
        self.assertIn("hostStack.address is required",
                      render_error(OBSERVABILITY, "--set", "clusterName=c"))
        self.assertIn("clusterName is required",
                      render_error(OBSERVABILITY, "--set", "hostStack.address=10.0.0.1"))

    def test_metrics_and_logs_go_to_the_host_stack(self) -> None:
        docs = render(OBSERVABILITY, *KIND, "--set", "hostStack.address=10.1.2.3",
                      "--set", "hostStack.logsPort=9500")
        args = container(find(docs, "Deployment", "vmagent"), "vmagent")["args"]
        self.assertIn("-remoteWrite.url=http://10.1.2.3:8428/api/v1/write", args)
        conf = find(docs, "ConfigMap", "fluent-bit-config")["data"]["fluent-bit.conf"]
        self.assertRegex(conf, r"Host\s+10\.1\.2\.3\n\s+Port\s+9500\n")

    def test_scrape_config_labels_the_cluster_and_takes_extra_jobs(self) -> None:
        docs = render(OBSERVABILITY, *KIND, "--set", "vmagent.extraScrapeConfigs[0].job_name=gen",
                      "--set", "vmagent.extraScrapeConfigs[0].static_configs[0].targets[0]=g:9")
        conf = yaml.safe_load(find(docs, "ConfigMap", "vmagent-config")["data"]["vmagent.yaml"])
        self.assertEqual(conf["global"]["external_labels"], {"cluster": "kind-sip-e2e"})
        jobs = [j["job_name"] for j in conf["scrape_configs"]]
        self.assertEqual(jobs, ["kubernetes-pods", "kubernetes-kubelet", "kubernetes-cadvisor",
                                "vmagent", "gen"])

    def test_a_log_config_change_rolls_the_shippers(self) -> None:
        def checksum(*args: str) -> str:
            ds = find(render(OBSERVABILITY, *KIND, *args), "DaemonSet", "fluent-bit")
            return str(ds["spec"]["template"]["metadata"]["annotations"]["checksum/config"])
        self.assertNotEqual(checksum(), checksum("--set", "hostStack.logsPort=9500"))


if __name__ == "__main__":
    unittest.main()
