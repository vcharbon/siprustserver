"""The siprust chart as a dependency of a downstream product chart: the
extension points a product uses (its own worker runner, a media relay sidecar,
env by name, per-component sizing) reach the render through the parent's
values. A default env is removed by a null in user-supplied values (-f,
--set); Helm keeps a subchart default a parent chart's own values.yaml sets
to null, so there a product overrides it."""
import shutil
import tempfile
import unittest
from pathlib import Path
from typing import Any

import yaml

from charts import SIPRUST, Doc, container, env, find, helm, names, pod_spec

PRODUCT_VALUES: dict[str, Any] = {
    "siprust": {
        "network": yaml.safe_load((SIPRUST / "values-kind.yaml").read_text())["network"],
        "worker": {
            "image": {"repository": "product-worker", "tag": "1.2.3"},
            "command": ["/usr/local/bin/product-runner"],
            "env": {
                "DECISION_URL": "http://decisions:8081",
                "B2BUA_STRESS_LIMITER_ID": "",
                "B2BUA_KEEPALIVE_SEC": "600",
            },
            "extraEnv": [{"name": "NODE_NAME",
                          "valueFrom": {"fieldRef": {"fieldPath": "spec.nodeName"}}}],
            "extraContainers": [{
                "name": "media-relay",
                "image": "media-relay:0.1",
                "ports": [{"name": "rtp", "containerPort": 30000, "protocol": "UDP"}],
                "volumeMounts": [{"name": "relay-state", "mountPath": "/var/lib/relay"}],
            }],
            "extraVolumes": [{"name": "relay-state", "emptyDir": {}}],
            "resources": {"requests": {"cpu": "1"}, "limits": {"memory": "3Gi"}},
        },
        "proxy": {
            "recvShards": 4,
            "selfGate": {"cpsRate": 1000, "cpsSize": 500},
            "resources": {"requests": {"cpu": "2", "memory": "64Mi"}},
        },
        "cdr": {"enabled": False},
    },
}
LAB_VALUES = {"siprust": {"worker": {"env": {"B2BUA_STRESS_LIMITER_LIMIT": None}}}}


class DownstreamChart(unittest.TestCase):
    docs: list[Doc] = []

    @classmethod
    def setUpClass(cls) -> None:
        root = Path(tempfile.mkdtemp(prefix="helm-downstream-"))
        cls.addClassCleanup(shutil.rmtree, root)
        chart = root / "product"
        chart.mkdir()
        (chart / "Chart.yaml").write_text(yaml.safe_dump({
            "apiVersion": "v2", "name": "product", "version": "0.1.0",
            "dependencies": [{"name": "siprust", "version": "0.1.0",
                              "repository": f"file://{SIPRUST}"}],
        }))
        (chart / "values.yaml").write_text(yaml.safe_dump(PRODUCT_VALUES))
        built = helm("dependency", "build", str(chart))
        if built.returncode != 0:
            raise AssertionError(f"helm dependency build failed:\n{built.stderr}")
        (root / "lab.yaml").write_text(yaml.safe_dump(LAB_VALUES))
        out = helm("template", "product", str(chart), "-n", "product-lab",
                   "-f", str(root / "lab.yaml"))
        if out.returncode != 0:
            raise AssertionError(f"helm template product failed:\n{out.stderr}")
        cls.docs = [d for d in yaml.safe_load_all(out.stdout) if d]

    def worker(self) -> Doc:
        return find(self.docs, "StatefulSet", "b2bua-worker")

    def test_the_product_runs_its_own_worker_runner(self) -> None:
        c = container(self.worker(), "b2bua-worker")
        self.assertEqual(c["image"], "product-worker:1.2.3")
        self.assertEqual(c["imagePullPolicy"], "IfNotPresent")
        self.assertEqual(c["command"], ["/usr/local/bin/product-runner"])

    def test_env_by_name_adds_overrides_and_removes(self) -> None:
        c = container(self.worker(), "b2bua-worker")
        e = env(c)
        self.assertEqual(e["DECISION_URL"], "http://decisions:8081")
        self.assertEqual(e["B2BUA_KEEPALIVE_SEC"], "600")
        self.assertEqual(e["B2BUA_STRESS_LIMITER_ID"], "")
        self.assertNotIn("B2BUA_STRESS_LIMITER_LIMIT", e)
        self.assertEqual(c["env"][-1]["name"], "NODE_NAME")

    def test_a_media_relay_sidecar_rides_the_worker_pod(self) -> None:
        spec = pod_spec(self.worker())
        self.assertEqual([c["name"] for c in spec["containers"]], ["b2bua-worker", "media-relay"])
        self.assertEqual(spec["volumes"], [{"name": "relay-state", "emptyDir": {}}])
        self.assertEqual(container(self.worker(), "b2bua-worker")["resources"],
                         {"requests": {"cpu": "1", "memory": "128Mi"}, "limits": {"memory": "3Gi"}})

    def test_the_proxy_is_sized_for_the_product(self) -> None:
        c = container(find(self.docs, "Deployment", "sip-front-proxy"), "sip-front-proxy")
        e = env(c)
        self.assertEqual((e["PROXY_RECV_SHARDS"], e["PROXY_SELF_GATE_CPS_RATE"],
                          e["PROXY_SELF_GATE_CPS_SIZE"]), ("4", "1000", "500"))
        self.assertEqual(c["resources"]["requests"]["cpu"], "2")

    def test_a_backend_the_product_leaves_out_is_gone(self) -> None:
        self.assertNotIn(("Deployment", "rabbitmq"), names(self.docs))
        self.assertTrue(all(d["metadata"].get("namespace", "product-lab") == "product-lab"
                            for d in self.docs))


if __name__ == "__main__":
    unittest.main()
