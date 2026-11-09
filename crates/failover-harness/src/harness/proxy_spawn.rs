//! The front proxy SUTs a harness stands up: one load-balancing proxy over the
//! workers, or a dual-face one, each reading the wire under the harness's
//! proxy limits.

use std::net::SocketAddr;

use b2bua_harness::spawn_proxy_reading;
use observe::NodeScope;

use super::{FailoverHarness, ProxySut};

impl FailoverHarness {
    /// Every proxy spawned after this reads the wire under `limits`: the
    /// exemptions a deployment names among them (ADR-0001).
    pub fn with_proxy_limits(mut self, limits: sip_message::SipParserLimits) -> Self {
        self.proxy_limits = limits;
        self
    }

    /// Stand up the real load-balancing proxy SUT at `addr` over a
    /// [`SimulatedWorkerRegistry`](sip_proxy::registry::simulated::SimulatedWorkerRegistry)
    /// holding both workers (alive). HRW selection keys off Call-ID + the alive
    /// set; the scenario marks a worker dead/alive via [`ProxySut::set_health`].
    pub async fn spawn_proxy(&self, addr: &str, workers: &[(&str, SocketAddr)]) -> ProxySut {
        self.spawn_proxy_inner(addr, workers, None).await
    }

    pub(super) async fn spawn_proxy_inner(
        &self,
        addr: &str,
        workers: &[(&str, SocketAddr)],
        probe: Option<(&str, sip_proxy::health::HealthProbeConfig)>,
    ) -> ProxySut {
        // The registry→hmac→observer→strategy→ProxyCore wiring is the shared
        // `b2bua_harness::spawn_proxy_core` primitive (ADR-0013 §0): bind the
        // proxy endpoint here, then hand it the multi-worker slice + this
        // harness's shared clock. The returned parts carry the CONCRETE registry
        // (retained below so `set_health`/`set_address` drive the live proxy) and
        // the SAME load observer the strategy reads (fed by the probe's
        // `X-Overload` payloads below — failover-only).
        let (ep, sock) = self.harness.bind_sut("proxy", addr).await;
        let node = NodeScope::named("proxy");
        let limits = self.proxy_limits.clone();
        let parts = node.enter(|| {
            spawn_proxy_reading(ep, sock, None, workers, self.clock.clone(), 0xC0FFEE, limits)
        });
        let b2bua_harness::ProxyCoreParts { addr: sock, registry, metrics, observer, task } = parts;

        let probe_task = match probe {
            Some((at, cadence)) => {
                Some(self.spawn_health_probe(&node, at, cadence, &registry, observer).await)
            }
            None => None,
        };

        let notices = self.spawn_dead_notifier(&node, sock, &registry).await;
        // The registry is the `proxy` observer's source — the ledger samples it
        // for presence ⊕ health after every advance chunk.
        self.views.attach_proxy(registry.clone());
        ProxySut { addr: sock, ext_addr: None, registry, metrics, task, probe_task, notices, node }
    }

    /// Stand up a **dual-face** load-balancing proxy SUT: the internal face at
    /// `int_addr` (workers' outbound-proxy target, worker plane), the external
    /// face at `ext_addr` (what callers dial). `name` prefixes the two bind
    /// lanes (`{name}-int` / `{name}-ext`); `id_seed` seeds the proxy's Via-branch generator — a
    /// takeover twin re-bound on the SAME VIPs must use a different seed so it
    /// cannot replay the dead proxy's branch sequence into live dialogs.
    ///
    /// Both faces are registered as **relay lanes**: the Drop-time
    /// endpoint-scoped RFC audit excludes them (same rationale as worker
    /// binds — a dual-face relay splits one dialog's stream across its two
    /// binds, so neither face alone is an endpoint-shaped stream; conformance
    /// is judged at alice/bob, which see everything the proxy emits). The
    /// lanes are role-tagged `{Proxy}` so the full-suite audit
    /// ([`assert_full_rfc_clean`](Self::assert_full_rfc_clean)) judges them
    /// under the proxy-subject rules only.
    pub async fn spawn_proxy_dual(
        &mut self,
        name: &str,
        int_addr: &str,
        ext_addr: &str,
        workers: &[(&str, SocketAddr)],
        id_seed: u64,
    ) -> ProxySut {
        let roles = std::collections::HashSet::from([sip_net::UaRole::Proxy]);
        let (int_ep, int_sock) =
            self.harness.bind_sut_with_roles(&format!("{name}-int"), int_addr, roles.clone()).await;
        let (ext_ep, ext_sock) =
            self.harness.bind_sut_with_roles(&format!("{name}-ext"), ext_addr, roles).await;
        self.relay_lanes.push(int_sock.to_string());
        self.relay_lanes.push(ext_sock.to_string());

        let external = b2bua_harness::ExternalProxyFace { endpoint: ext_ep, addr: ext_sock };
        let node = NodeScope::named(name);
        let parts = node.enter(|| {
            b2bua_harness::spawn_proxy_reading(
                int_ep,
                int_sock,
                Some(external),
                workers,
                self.clock.clone(),
                id_seed,
                self.proxy_limits.clone(),
            )
        });
        let b2bua_harness::ProxyCoreParts { addr: sock, registry, metrics, observer: _, task } =
            parts;
        let notices = self.spawn_dead_notifier(&node, sock, &registry).await;
        ProxySut {
            addr: sock,
            ext_addr: Some(ext_sock),
            registry,
            metrics,
            task,
            probe_task: None,
            notices,
            node,
        }
    }
}
