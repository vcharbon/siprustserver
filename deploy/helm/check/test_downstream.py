"""The siprust chart as a dependency of a downstream product chart: the
extension points a product uses (its own worker runner, a media relay beside
the proxy on the external plane, env by name, per-component sizing, the
dependency's condition) reach the render through the parent's values. A null
in user-supplied values (-f, --set) removes an env the parent sets; Helm keeps
a subchart default a parent chart's own values.yaml sets to null, so there a
product overrides it."""
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any

import yaml

from charts import HELM_DIR, SIPRUST, Doc, container, env, find, helm, names, pod_spec

PRODUCT_VALUES: dict[str, Any] = {
    "siprust": {
        "network": yaml.safe_load((SIPRUST / "values-kind.yaml").read_text())["network"],
        "worker": {
            "image": {"repository": "product-worker", "tag": "1.2.3"},
            "command": ["/usr/local/bin/product-runner"],
            "env": {"DECISION_URL": "http://decisions:8081", "B2BUA_KEEPALIVE_SEC": "600",
                    "B2BUA_STRESS_LIMITER_ID": ""},
            "extraEnv": [{"name": "NODE_NAME",
                          "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}}],
            "resources": {"requests": {"cpu": "1"}, "limits": {"memory": "3Gi"}},
        },
        "proxy": {
            "recvShards": 4,
            "selfGate": {"cpsRate": 1000, "cpsSize": 500},
            "resources": {"requests": {"cpu": "2", "memory": "64Mi"}},
            "keepalived": {
                "extraVirtualIps": [{"address": "192.168.60.240", "interface": "eth1"}],
                "extraTrackScripts": [{"name": "chk_relay", "script": "/usr/bin/relay-ok"}],
            },
            "extraContainers": [{
                "name": "media-relay",
                "image": "media-relay:0.1",
                "args": ["--listen", "192.168.60.240"],
                "ports": [{"name": "rtp", "containerPort": 30000, "protocol": "UDP"}],
                "volumeMounts": [{"name": "relay-state", "mountPath": "/var/lib/relay"}],
            }],
            "extraVolumes": [{"name": "relay-state", "emptyDir": {}}],
        },
        "cdr": {"enabled": False},
    },
}
LAB_VALUES = {"siprust": {"worker": {"env": {"B2BUA_KEEPALIVE_SEC": None}}}}


class DownstreamChart(unittest.TestCase):
    chart = Path()
    root = Path()
    docs: list[Doc] = []

    @classmethod
    def setUpClass(cls) -> None:
        cls.root = Path(tempfile.mkdtemp(prefix="helm-downstream-"))
        cls.addClassCleanup(shutil.rmtree, cls.root)
        cls.chart = cls.root / "product"
        cls.chart.mkdir()
        (cls.chart / "Chart.yaml").write_text(yaml.safe_dump({
            "apiVersion": "v2", "name": "product", "version": "0.1.0",
            "dependencies": [{"name": "siprust", "version": "0.1.0",
                              "repository": f"file://{SIPRUST}",
                              "condition": "siprust.enabled"}],
        }))
        (cls.chart / "values.yaml").write_text(yaml.safe_dump(PRODUCT_VALUES))
        built = helm("dependency", "build", str(cls.chart))
        if built.returncode != 0:
            raise AssertionError(f"helm dependency build failed:\n{built.stderr}")
        (cls.root / "lab.yaml").write_text(yaml.safe_dump(LAB_VALUES))
        cls.docs = cls.render("-f", str(cls.root / "lab.yaml"))

    @classmethod
    def render(cls, *args: str) -> list[Doc]:
        out = helm("template", "product", str(cls.chart), "-n", "product-lab", *args)
        if out.returncode != 0:
            raise AssertionError(f"helm template product failed:\n{out.stderr}")
        return [d for d in yaml.safe_load_all(out.stdout) if d]

    def worker(self) -> Doc:
        return find(self.docs, "StatefulSet", "b2bua-worker")

    def proxy(self) -> Doc:
        return find(self.docs, "Deployment", "sip-front-proxy")

    def test_the_product_runs_its_own_worker_runner(self) -> None:
        c = container(self.worker(), "b2bua-worker")
        self.assertEqual(c["image"], "product-worker:1.2.3")
        self.assertEqual(c["imagePullPolicy"], "IfNotPresent")
        self.assertEqual(c["command"], ["/usr/local/bin/product-runner"])
        self.assertEqual(c["resources"],
                         {"requests": {"cpu": "1", "memory": "128Mi"}, "limits": {"memory": "3Gi"}})

    def test_env_by_name_adds_and_a_user_null_removes(self) -> None:
        c = container(self.worker(), "b2bua-worker")
        e = env(c)
        self.assertEqual(e["DECISION_URL"], "http://decisions:8081")
        self.assertEqual(e["B2BUA_STRESS_LIMITER_ID"], "")
        self.assertNotIn("B2BUA_KEEPALIVE_SEC", e)
        self.assertEqual(c["env"][-1]["name"], "NODE_NAME")

    def test_a_media_relay_rides_the_proxy_pod_on_its_own_floating_address(self) -> None:
        spec = pod_spec(self.proxy())
        self.assertEqual([c["name"] for c in spec["containers"]],
                         ["sip-front-proxy", "keepalived", "media-relay"])
        self.assertIn({"name": "relay-state", "emptyDir": {}}, spec["volumes"])
        self.assertTrue(spec["hostNetwork"])
        self.assertEqual(env(container(self.proxy(), "vip-loopback"))["EXTRA_VIPS"],
                         "192.168.60.240")
        conf = find(self.docs, "ConfigMap", "sip-front-proxy-keepalived")["data"]["keepalived.conf"]
        self.assertIn("192.168.60.240 dev eth1", conf)
        self.assertIn("vrrp_script chk_relay {", conf)

    def test_the_proxy_is_sized_for_the_product(self) -> None:
        c = container(self.proxy(), "sip-front-proxy")
        e = env(c)
        self.assertEqual((e["PROXY_RECV_SHARDS"], e["PROXY_SELF_GATE_CPS_RATE"],
                          e["PROXY_SELF_GATE_CPS_SIZE"]), ("4", "1000", "500"))
        self.assertEqual(c["resources"]["requests"]["cpu"], "2")

    def test_a_backend_the_product_leaves_out_is_gone(self) -> None:
        self.assertNotIn(("Deployment", "rabbitmq"), names(self.docs))
        self.assertTrue(all(d["metadata"].get("namespace", "product-lab") == "product-lab"
                            for d in self.docs))

    def test_the_dependency_condition_drops_the_whole_chart(self) -> None:
        self.assertEqual(self.render("--set", "siprust.enabled=false"), [])


class DownstreamKindLab(unittest.TestCase):
    """kind-lab.sh deploys a product chart as the SUT release: the siprust kind
    values nested under the dependency, the product's values-kind.yaml over
    them, the helm args over both."""

    def test_the_value_set_nests_the_lab_values_under_the_dependency_in_order(self) -> None:
        root = Path(tempfile.mkdtemp(prefix="helm-downstream-lab-"))
        self.addCleanup(shutil.rmtree, root)
        chart = root / "product"
        chart.mkdir()
        (chart / "Chart.yaml").write_text(yaml.safe_dump({
            "apiVersion": "v2", "name": "product", "version": "0.1.0",
            "dependencies": [{"name": "siprust", "version": "0.1.0",
                              "repository": f"file://{SIPRUST}"}],
        }))
        (chart / "values.yaml").write_text(yaml.safe_dump(
            {"siprust": {"worker": {"command": ["/usr/local/bin/product-runner"]}}}))
        (chart / "values-kind.yaml").write_text(yaml.safe_dump(
            {"siprust": {"worker": {"env": {"B2BUA_KEEPALIVE_SEC": "700"}}}}))
        out = subprocess.run(
            [str(HELM_DIR / "kind-lab.sh"), "template", "--set", "siprust.worker.replicas=3"],
            capture_output=True, text=True,
            env={**os.environ, "SUT_CHART": str(chart), "SUT_RELEASE": "product",
                 "SUT_SUBCHART": "siprust"})
        self.assertEqual(out.returncode, 0, out.stderr)
        docs: list[Doc] = [d for d in yaml.safe_load_all(out.stdout) if d]
        worker = find(docs, "StatefulSet", "b2bua-worker")
        e = env(container(worker, "b2bua-worker"))
        self.assertEqual(e["B2BUA_UDP_SNDBUF"], "4194304", "the nested kind values apply")
        self.assertEqual(e["B2BUA_KEEPALIVE_SEC"], "700", "the product's kind values win")
        self.assertEqual(worker["spec"]["replicas"], 3, "the helm args win")
        self.assertIn(("DaemonSet", "masq-exempt"), names(docs))


if __name__ == "__main__":
    unittest.main()
