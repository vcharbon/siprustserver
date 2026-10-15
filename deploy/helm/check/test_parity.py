"""The kind lab from the chart against deploy/k8s/manifests rendered the
run.sh way (envsubst over net-env.sh and run.sh's defaults, replication on):
the fields that keep a call or a node safe agree, workload by workload.
This check lives until the cutover deletes the old manifests."""
import os
import subprocess
import unittest
from typing import Any, cast

import yaml

from charts import NET_ENV, SIPRUST, Doc, kind_values, render, workloads

MANIFESTS = SIPRUST.parent.parent / "k8s" / "manifests"
RUN_SH_ENV = {"SUT_IMAGE": "siprustserver:dev", "WORKER_REPLICAS": "2", "REPL_ENABLE": "1",
              "REPL_PORT": "9092", "RABBITMQ_IMAGE": "rabbitmq:3.13-management",
              "CDR_CONSUMER_PAYLOAD": "json", "SIP_TRACE_HEADER": "1"}
POD_FIELDS = ("nodeSelector", "tolerations", "hostNetwork", "dnsPolicy", "serviceAccountName",
              "terminationGracePeriodSeconds", "securityContext")
CONTAINER_FIELDS = ("image", "imagePullPolicy", "command", "readinessProbe", "livenessProbe",
                    "resources", "securityContext", "lifecycle", "volumeMounts")
# Their script reads its settings from env in the chart, where the old manifest
# inlined them through envsubst: the text and the env differ by design.
SCRIPT_CONTAINERS = {"vip-loopback", "masq-exempt"}
# Settings the chart adds that the old manifests never had.
CHART_ONLY_ENV = {"cdr-consumer": {"CDR_OBSERVATION_URL"}}


def old_render() -> list[Doc]:
    """Every manifest through envsubst, as run.sh's subst_manifest does."""
    script = (f'. "{NET_ENV}"; export OTLP_EXPORT_ENDPOINT="http://$SIP_GATEWAY:10428/insert/opentelemetry"; '
              f'for f in "{MANIFESTS}"/*.yaml; do envsubst < "$f"; echo ---; done')
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, check=True,
                         env={"PATH": os.environ["PATH"], **RUN_SH_ENV})
    return [cast(Doc, d) for d in yaml.safe_load_all(out.stdout) if d]


def by_name(docs: list[Doc]) -> dict[tuple[str, str], Doc]:
    return {(d["kind"], d["metadata"]["name"]): d for d in docs}


def containers(w: Doc) -> dict[str, dict[str, Any]]:
    spec = w["spec"]["template"]["spec"]
    return {c["name"]: c for c in [*spec.get("initContainers", []), *spec["containers"]]}


def env_of(c: dict[str, Any]) -> dict[str, Any]:
    return {e["name"]: e.get("value", e.get("valueFrom")) for e in c.get("env", [])}


def conf_lines(text: str) -> list[str]:
    """A keepalived config's statements, comments and spacing left out."""
    lines = (" ".join(ln.split()) for ln in text.splitlines())
    return [ln for ln in lines if ln and not ln.startswith("#")]


class Parity(unittest.TestCase):
    old: dict[tuple[str, str], Doc] = {}
    new: dict[tuple[str, str], Doc] = {}

    @classmethod
    def setUpClass(cls) -> None:
        cls.old = by_name(old_render())
        cls.new = by_name(render(SIPRUST, *kind_values(SIPRUST)))

    def test_the_chart_renders_every_old_workload(self) -> None:
        self.assertEqual({k for k in self.old if k[0] in ("Deployment", "StatefulSet", "DaemonSet")},
                         {(w["kind"], w["metadata"]["name"]) for w in workloads(list(self.new.values()))})

    def test_pods_and_containers_agree(self) -> None:
        for key, old in self.old.items():
            if key[0] not in ("Deployment", "StatefulSet", "DaemonSet"):
                continue
            new = self.new[key]
            with self.subTest(workload=key):
                self.assertEqual(new["spec"].get("replicas"), old["spec"].get("replicas"))
                self.assertEqual(new["spec"].get("strategy"), old["spec"].get("strategy"))
                old_pod, new_pod = old["spec"]["template"]["spec"], new["spec"]["template"]["spec"]
                for field in POD_FIELDS:
                    self.assertEqual(new_pod.get(field), old_pod.get(field), field)
                self.assertEqual(new_pod.get("affinity", {}).get("podAntiAffinity"),
                                 old_pod.get("affinity", {}).get("podAntiAffinity"))
                old_ann = old["spec"]["template"]["metadata"].get("annotations", {})
                new_ann = new["spec"]["template"]["metadata"].get("annotations", {})
                self.assertEqual({k: v for k, v in new_ann.items() if k.startswith("prometheus.io/")},
                                 old_ann)
                old_cs, new_cs = containers(old), containers(new)
                self.assertEqual(set(new_cs), set(old_cs))
                for name, oc in old_cs.items():
                    nc = new_cs[name]
                    for field in CONTAINER_FIELDS:
                        self.assertEqual(nc.get(field), oc.get(field), f"{name}.{field}")
                    if name not in SCRIPT_CONTAINERS:
                        added = CHART_ONLY_ENV.get(name, set())
                        new_env = {k: v for k, v in env_of(nc).items() if k not in added}
                        self.assertEqual(new_env, env_of(oc), f"{name}.env")
                        self.assertEqual(nc.get("args"), oc.get("args"), f"{name}.args")

    def test_services_expose_the_same_ports(self) -> None:
        for key, old in self.old.items():
            if key[0] == "Service":
                with self.subTest(service=key[1]):
                    new = self.new[key]
                    self.assertEqual(new["spec"].get("ports"), old["spec"].get("ports"))
                    self.assertEqual(new["spec"].get("clusterIP"), old["spec"].get("clusterIP"))

    def test_the_keepalived_config_states_the_same(self) -> None:
        key = ("ConfigMap", "sip-front-proxy-keepalived")
        self.assertEqual(conf_lines(self.new[key]["data"]["keepalived.conf"]),
                         conf_lines(self.old[key]["data"]["keepalived.conf"]))

    def test_rbac_grants_the_same(self) -> None:
        for key, old in self.old.items():
            if key[0] in ("Role", "RoleBinding"):
                with self.subTest(rbac=key):
                    new = self.new[key]
                    self.assertEqual(new.get("rules"), old.get("rules"))
                    self.assertEqual(new.get("roleRef"), old.get("roleRef"))
                    self.assertEqual(new.get("subjects"), old.get("subjects"))


if __name__ == "__main__":
    unittest.main()
