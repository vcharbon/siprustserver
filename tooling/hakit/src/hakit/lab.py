"""The facts of the lab a run drives, read from the environment.

The defaults describe the base's own kind lab (`deploy/k8s/lib/net-env.sh`):
the generators run as docker containers on the external SIP bridge and dial
the front proxy's virtual address; the host observability stack serves
VictoriaMetrics and VictoriaLogs.
"""

from __future__ import annotations

from collections.abc import Mapping
from dataclasses import dataclass


@dataclass(frozen=True)
class Lab:
    kube_context: str
    namespace: str
    capabilities: list[str]
    vm_url: str
    vlogs_url: str
    # VictoriaLogs as a generator container reaches it on the generator network.
    vlogs_url_from_generators: str
    sipp_image: str
    sipp_network: str
    sip_target: str
    sipp_ip_base: str
    sipp_ip_slots: int
    callees: list[tuple[str, int]]
    leftover_calls_query: str

    @property
    def vm_import_url(self) -> str:
        return f"{self.vm_url}/api/v1/import/prometheus"

    def sipp_ip(self, slot: int) -> str:
        if not 0 <= slot < self.sipp_ip_slots:
            raise ValueError(f"generator slot {slot} is outside 0..{self.sipp_ip_slots - 1}")
        prefix, last = self.sipp_ip_base.rsplit(".", 1)
        return f"{prefix}.{int(last) + slot}"


def _callees(raw: str) -> list[tuple[str, int]]:
    out: list[tuple[str, int]] = []
    for item in raw.split(","):
        item = item.strip()
        if not item:
            continue
        host, _, port = item.partition(":")
        out.append((host, int(port or "5060")))
    return out


def lab_from_env(env: Mapping[str, str]) -> Lab:
    g = env.get
    return Lab(
        kube_context=g("HAKIT_KUBE_CONTEXT", "kind-sip-e2e"),
        namespace=g("HAKIT_NAMESPACE", "sip-test"),
        capabilities=[c for c in g("HAKIT_LAB_CAPABILITIES", "kind,docker").split(",") if c],
        vm_url=g("HAKIT_VM_URL", "http://127.0.0.1:8428"),
        vlogs_url=g("HAKIT_VLOGS_URL", "http://127.0.0.1:9428"),
        vlogs_url_from_generators=g("HAKIT_VLOGS_URL_GENERATORS", "http://victorialogs:9428"),
        sipp_image=g("HAKIT_SIPP_IMAGE", "sipp:dev"),
        sipp_network=g("HAKIT_SIPP_NETWORK", "sipext"),
        sip_target=g("HAKIT_SIP_TARGET", "192.168.60.250:5060"),
        sipp_ip_base=g("HAKIT_SIPP_IP_BASE", "192.168.60.100"),
        sipp_ip_slots=int(g("HAKIT_SIPP_IP_SLOTS", "36")),
        callees=_callees(g("HAKIT_CALLEES", "192.168.60.21:5060,192.168.60.22:5060")),
        leftover_calls_query=g("HAKIT_LEFTOVER_CALLS_QUERY", "sum(last_over_time(b2bua_active_calls[30s]))"),
    )
