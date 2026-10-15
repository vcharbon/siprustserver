"""Rendering helpers of the chart checks: helm on the PATH, PyYAML."""
import base64
import os
import subprocess
from pathlib import Path
from typing import Any, cast

import yaml

HELM_DIR = Path(__file__).resolve().parent.parent
SIPRUST = HELM_DIR / "siprust"
OBSERVABILITY = HELM_DIR / "siprust-observability"
NET_ENV = HELM_DIR.parent / "k8s" / "lib" / "net-env.sh"
GOLDEN = Path(__file__).resolve().parent / "golden"

Doc = dict[str, Any]


def helm(*args: str, cwd: Path | None = None) -> subprocess.CompletedProcess[str]:
    """One helm call, its output captured; never raises on a non-zero exit."""
    return subprocess.run(["helm", *args], capture_output=True, text=True, cwd=cwd)


def render_text(chart: Path, *args: str, namespace: str = "sip-test") -> str:
    out = helm("template", "rel", str(chart), "-n", namespace, *args)
    if out.returncode != 0:
        raise AssertionError(f"helm template {chart.name} {' '.join(args)} failed:\n{out.stderr}")
    return out.stdout


def render(chart: Path, *args: str, namespace: str = "sip-test") -> list[Doc]:
    """The objects `helm template` renders, empty documents dropped."""
    docs = yaml.safe_load_all(render_text(chart, *args, namespace=namespace))
    return [cast(Doc, d) for d in docs if d]


def render_error(chart: Path, *args: str) -> str:
    """The stderr of a render that must fail."""
    out = helm("template", "rel", str(chart), *args)
    if out.returncode == 0:
        raise AssertionError(f"helm template {chart.name} {' '.join(args)} rendered")
    return out.stderr


def kind_values(chart: Path) -> list[str]:
    return ["-f", str(chart / "values-kind.yaml")]


def find(docs: list[Doc], kind: str, name: str) -> Doc:
    for d in docs:
        if d["kind"] == kind and d["metadata"]["name"] == name:
            return d
    raise AssertionError(f"no {kind}/{name} rendered")


def names(docs: list[Doc]) -> set[tuple[str, str]]:
    return {(d["kind"], d["metadata"]["name"]) for d in docs}


def pod_spec(workload: Doc) -> Doc:
    return cast(Doc, workload["spec"]["template"]["spec"])


def container(workload: Doc, name: str) -> Doc:
    spec = pod_spec(workload)
    for c in [*spec.get("initContainers", []), *spec["containers"]]:
        if c["name"] == name:
            return cast(Doc, c)
    raise AssertionError(f"no container {name} in {workload['metadata']['name']}")


def env(c: Doc) -> dict[str, str]:
    """The plain-value env of a container by name."""
    return {e["name"]: e["value"] for e in c.get("env", []) if "value" in e}


def workloads(docs: list[Doc]) -> list[Doc]:
    return [d for d in docs if d["kind"] in ("Deployment", "StatefulSet", "DaemonSet")]


def secret_key_bytes(secret: Doc) -> bytes:
    """The signing key the proxy reads: the Secret's value is base64 text of the bytes."""
    text = base64.b64decode(secret["data"]["key"])
    return base64.b64decode(text)


def canonical(docs: list[Doc]) -> str:
    """A render as sorted, key-sorted YAML, the random cookie key masked."""
    out: list[Doc] = []
    for d in sorted(docs, key=lambda d: (d["kind"], d["metadata"]["name"])):
        if d["kind"] == "Secret" and d["metadata"]["name"] == "sip-front-proxy-hmac":
            d = dict(d, data=dict(d["data"], key="<random>"))
        out.append(d)
    return yaml.safe_dump_all(out, sort_keys=True)


def net_env_defaults() -> dict[str, str]:
    """net-env.sh's values in a clean environment (no caller override)."""
    names = ["SIP_GATEWAY", "PROXY_VIP", "SIP_PORT", "SIPEXT_VIP", "SIPEXT_SUBNET", "SIP_IF",
             "SIPEXT_IF", "PROXY_FACE_INT_CIDRS", "SIPEXT_UAS_IP"]
    script = f'. "{NET_ENV}"; ' + "; ".join(f'echo "{n}=${n}"' for n in names)
    out = subprocess.run(["bash", "-c", script], capture_output=True, text=True, check=True,
                         env={"PATH": os.environ["PATH"]})
    return dict(line.split("=", 1) for line in out.stdout.splitlines())
