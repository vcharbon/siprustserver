"""The facts of the lab a run drives, read from the environment.

The defaults describe the base's own kind lab (`deploy/k8s/lib/net-env.sh`):
the generators run as docker containers on the external SIP bridge and dial
the front proxy's virtual address, or as pods on the edge nodes for a stream
placed in the cluster; the host observability stack serves VictoriaMetrics and
VictoriaLogs, which the pods reach through the kind bridge's gateway.
"""

from __future__ import annotations

import os
import re
from collections.abc import Mapping
from dataclasses import dataclass

from .executor import Executor


@dataclass(frozen=True)
class Lab:
    kube_context: str
    namespace: str
    capabilities: list[str]
    vm_url: str
    vlogs_url: str
    # VictoriaLogs as a generator container reaches it on the generator network.
    vlogs_url_from_generators: str
    # VictoriaLogs as a pod reaches it, and the nodes a pod-placed generator
    # runs on (a label selector; empty: any schedulable node).
    vlogs_url_from_pods: str
    pod_node_selector: str
    sipp_image: str
    sipp_network: str
    sip_target: str
    sipp_ip_base: str
    sipp_ip_slots: int
    callees: list[tuple[str, int]]
    leftover_calls_query: str
    # VictoriaMetrics selectors the run reads live, checked for freshness.
    live_series: list[str]
    # The CDR chain's counters over the run, `{span}` its length in seconds: the
    # CDRs the system under test wrote, those the consumer consumed, and the
    # payloads it could not read.
    cdr_written_query: str
    cdr_consumed_query: str
    cdr_unread_query: str
    # A tmpfs directory a docker generator works in, copied into the run
    # directory at its end (workdir.py); empty: the run directory itself.
    generator_scratch: str
    # The `uid:gid` a docker generator runs as: the runner's own, so the
    # files it writes can be collected and removed.
    generator_user: str

    @property
    def vm_import_url(self) -> str:
        return f"{self.vm_url}/api/v1/import/prometheus"

    def sipp_ip(self, slot: int) -> str:
        if not 0 <= slot < self.sipp_ip_slots:
            raise ValueError(f"generator slot {slot} is outside 0..{self.sipp_ip_slots - 1}")
        prefix, last = self.sipp_ip_base.rsplit(".", 1)
        return f"{prefix}.{int(last) + slot}"


def parse_callees(raw: str) -> list[tuple[str, int]]:
    out: list[tuple[str, int]] = []
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        host, _, port = item.partition(":")
        out.append((host, int(port or "5060")))
    return out


# Capabilities read from the lab itself, never declared: a capability whose
# backend is missing would fail its incidents at inject instead of refusing
# the profile.
DERIVED = ("chaos-mesh",)
CHAOS_MESH_CRD = "networkchaos.chaos-mesh.org"


def lab_capabilities(lab: Lab, executor: Executor) -> list[str]:
    """The declared capabilities plus those the lab shows: `chaos-mesh` when
    the API server serves Chaos Mesh's resources."""
    caps = list(lab.capabilities)
    res = executor.run(["kubectl", "--context", lab.kube_context, "get", "crd", CHAOS_MESH_CRD, "-o", "name"], timeout_s=30)
    if res.rc == 0 and res.stdout.strip():
        caps.append("chaos-mesh")
    return caps


_IPV4 = re.compile(r"^\d{1,3}(\.\d{1,3}){3}$")


def resolve_target(lab: Lab, executor: Executor, target: str | None) -> tuple[str | None, str | None]:
    """A stream's `host:port` with its host an IPv4 address: the lab's SIP
    target without one, a Service name resolved to its cluster IP. The
    address, or the reason it could not be resolved."""
    if target is None:
        return lab.sip_target, None
    host, _, port = target.rpartition(":")
    if _IPV4.match(host):
        return target, None
    res = executor.run(
        ["kubectl", "--context", lab.kube_context, "-n", lab.namespace, "get", "service", host]
        + ["-o", "jsonpath={.spec.clusterIP}"],
        timeout_s=30,
    )
    ip = res.stdout.strip()
    if res.rc != 0 or not _IPV4.match(ip):
        return None, f"target {target}: Service {host!r} has no cluster IP: {res.stderr.strip()[:200]}"
    return f"{ip}:{port}", None


def lab_from_env(env: Mapping[str, str]) -> Lab:
    g = env.get
    return Lab(
        kube_context=g("HAKIT_KUBE_CONTEXT", "kind-sip-e2e"),
        namespace=g("HAKIT_NAMESPACE", "sip-test"),
        capabilities=[c for c in g("HAKIT_LAB_CAPABILITIES", "kind,docker").split(",") if c and c not in DERIVED],
        vm_url=g("HAKIT_VM_URL", "http://127.0.0.1:8428"),
        vlogs_url=g("HAKIT_VLOGS_URL", "http://127.0.0.1:9428"),
        vlogs_url_from_generators=g("HAKIT_VLOGS_URL_GENERATORS", "http://victorialogs:9428"),
        vlogs_url_from_pods=g("HAKIT_VLOGS_URL_PODS", "http://172.20.0.1:9428"),
        pod_node_selector=g("HAKIT_POD_NODE_SELECTOR", "tier=edge"),
        sipp_image=g("HAKIT_SIPP_IMAGE", "sipp:dev"),
        sipp_network=g("HAKIT_SIPP_NETWORK", "sipext"),
        sip_target=g("HAKIT_SIP_TARGET", "192.168.60.250:5060"),
        sipp_ip_base=g("HAKIT_SIPP_IP_BASE", "192.168.60.100"),
        sipp_ip_slots=int(g("HAKIT_SIPP_IP_SLOTS", "36")),
        callees=parse_callees(g("HAKIT_CALLEES", "192.168.60.21:5060,192.168.60.22:5060")),
        leftover_calls_query=g("HAKIT_LEFTOVER_CALLS_QUERY", "sum(last_over_time(b2bua_active_calls[30s]))"),
        live_series=[s.strip() for s in g("HAKIT_LIVE_SERIES", "b2bua_active_calls").split(";") if s.strip()],
        cdr_written_query=g("HAKIT_CDR_WRITTEN_QUERY", "sum(increase(b2bua_cdr_written_total[{span}s]))"),
        cdr_consumed_query=g("HAKIT_CDR_CONSUMED_QUERY", "sum(increase(cdr_consumed_total[{span}s]))"),
        cdr_unread_query=g("HAKIT_CDR_UNREAD_QUERY", "sum(increase(cdr_observations_unread_total[{span}s]))"),
        generator_scratch=g("HAKIT_GENERATOR_SCRATCH", "/dev/shm/hakit"),
        generator_user=g("HAKIT_GENERATOR_USER", f"{os.getuid()}:{os.getgid()}"),
    )
