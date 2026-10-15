"""The kind lab values: lint-clean, agreeing with the docker bridges
net-env.sh lays out, and rendering what the golden files hold.

  HELM_GOLDEN_UPDATE=1 python3 -m unittest discover -s deploy/helm/check
rewrites the golden files from the current charts (`just helm-golden`).
"""
import difflib
import os
import unittest
from pathlib import Path

import yaml

from charts import (GOLDEN, OBSERVABILITY, SIPRUST, canonical, container, env, find, helm,
                    kind_values, net_env_defaults, render)


class Lint(unittest.TestCase):
    def test_both_charts_lint_clean_with_the_kind_values(self) -> None:
        for chart in (SIPRUST, OBSERVABILITY):
            out = helm("lint", "--strict", str(chart), *kind_values(chart))
            self.assertEqual(out.returncode, 0, out.stdout + out.stderr)

    def test_the_sut_chart_lints_without_a_proxy(self) -> None:
        out = helm("lint", "--strict", str(SIPRUST), "--set", "proxy.enabled=false")
        self.assertEqual(out.returncode, 0, out.stdout + out.stderr)


class NetEnvAgreement(unittest.TestCase):
    """values-kind.yaml restates net-env.sh's defaults; the two never drift."""

    def test_the_sut_values_match_the_bridges(self) -> None:
        net = net_env_defaults()
        values = yaml.safe_load((SIPRUST / "values-kind.yaml").read_text())
        n = values["network"]
        self.assertEqual(
            (n["proxyVip"], n["externalVip"], n["internalInterface"], n["externalInterface"],
             n["externalSubnet"], str(n["sipPort"]), n["internalCidrs"]),
            (net["PROXY_VIP"], net["SIPEXT_VIP"], net["SIP_IF"], net["SIPEXT_IF"],
             net["SIPEXT_SUBNET"], net["SIP_PORT"], net["PROXY_FACE_INT_CIDRS"]))
        self.assertEqual(values["worker"]["dest"], f"{net['SIPEXT_UAS_IP']}:5060")
        self.assertEqual(values["tracing"]["otlpEndpoint"],
                         f"http://{net['SIP_GATEWAY']}:10428/insert/opentelemetry")

    def test_the_collectors_reach_the_host_through_the_bridge_gateway(self) -> None:
        values = yaml.safe_load((OBSERVABILITY / "values-kind.yaml").read_text())
        self.assertEqual(values["hostStack"]["address"], net_env_defaults()["SIP_GATEWAY"])

    def test_the_worker_gets_the_lab_trace_settings(self) -> None:
        w = find(render(SIPRUST, *kind_values(SIPRUST)), "StatefulSet", "b2bua-worker")
        e = env(container(w, "b2bua-worker"))
        self.assertEqual(e["SIP_TRACE_HEADER"], "1")
        self.assertTrue(e["OTEL_EXPORTER_OTLP_ENDPOINT"].startswith("http://"))


class Golden(unittest.TestCase):
    def check(self, chart: Path, namespace: str) -> None:
        got = canonical(render(chart, *kind_values(chart), namespace=namespace))
        path = GOLDEN / f"{chart.name}-kind.yaml"
        if os.environ.get("HELM_GOLDEN_UPDATE") == "1":
            path.parent.mkdir(exist_ok=True)
            path.write_text(got)
            return
        want = path.read_text() if path.exists() else ""
        if got != want:
            diff = "".join(list(difflib.unified_diff(
                want.splitlines(True), got.splitlines(True), str(path), "render"))[:80])
            self.fail(f"{chart.name}: the kind render differs from {path.name} "
                      f"(intended? `just helm-golden`):\n{diff}")

    def test_the_sut_render(self) -> None:
        self.check(SIPRUST, "sip-test")

    def test_the_collectors_render(self) -> None:
        self.check(OBSERVABILITY, "observability")


if __name__ == "__main__":
    unittest.main()
