//! The orchestrator's hand on membership: withdrawing, flapping, departing
//! and re-admitting a worker in the proxy's registry and every live peer's
//! membership, each recorded on the views ledger.

use std::net::SocketAddr;

use sip_proxy::registry::WorkerHealth;
use sip_proxy::ProxyAddr;
use topology::Peer;

use super::FailoverHarness;
use crate::views::Belief;

impl FailoverHarness {
    /// **Withdraw `ordinal`'s endpoint** — the orchestrator takes the worker out
    /// of the cluster's view WITHOUT touching its process: the proxy's registry
    /// drops the ordinal (nothing routes to it, no cookie resolves it) and every
    /// other live worker's membership drops the peer (its supervisor parks the
    /// flows), and so does the worker's own — it observes its withdrawal and
    /// latches Draining (ADR-0031 D6). The process stays bound and keeps serving
    /// whatever still reaches it — the zombie shape. Records the orchestrator's,
    /// the proxy's and every peer's belief at this instant; the sampler then
    /// records what each component actually did with it.
    pub fn withdraw(&mut self, ordinal: &str) {
        self.mark(ordinal, None, "withdraw", "endpoint withdrawn; process still running");
        self.views.record("orchestrator", ordinal, Belief::Withdrawn, "withdraw");
        if let Some(registry) = self.views.proxy_registry() {
            registry.remove(ordinal);
            self.views.record("proxy", ordinal, Belief::Absent, "withdraw");
        }
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.remove(ordinal);
            self.views.record(&observer, ordinal, Belief::Absent, "withdraw");
        }
        for (_, membership) in self.views.live_incarnations_of(ordinal) {
            membership.remove(ordinal);
        }
    }

    /// **Withdraw `ordinal` from routing, keep it a replication peer** — the
    /// graceful shape (ADR-0031 case 1): the orchestrator has begun to remove the
    /// worker, so its endpoint reads `ready=false, terminating=true` but STAYS in
    /// the slice until the pod is gone. The proxy departs the ordinal (out of the
    /// projection, address tombstoned; no cookie resolves it) while every other
    /// live worker keeps pulling it on presence alone (D1). The process is
    /// untouched and keeps serving whatever still reaches it. Records the
    /// orchestrator's, the proxy's and every peer's belief at this instant.
    pub fn withdraw_routing(&mut self, ordinal: &str) {
        self.mark(
            ordinal,
            None,
            "withdraw_routing",
            "endpoint not ready + terminating, still in the slice; process still running",
        );
        self.views.record("orchestrator", ordinal, Belief::Withdrawn, "withdraw_routing");
        self.set_member_ready(ordinal, false, true, "withdraw_routing");
    }

    /// **Flap `ordinal` not ready at the same address** — the restarted-in-place
    /// shape (ADR-0031 case 4): a readiness probe fails, the endpoint reads
    /// `ready=false, terminating=false` and stays in the slice. The proxy departs
    /// the ordinal and tombstones its address; every other live worker keeps
    /// pulling it (D1). The process is untouched.
    pub fn flap_not_ready(&mut self, ordinal: &str) {
        self.mark(
            ordinal,
            None,
            "flap_not_ready",
            "endpoint not ready, same address, in the slice",
        );
        self.views.record("orchestrator", ordinal, Belief::Withdrawn, "flap_not_ready");
        self.set_member_ready(ordinal, false, false, "flap_not_ready");
    }

    /// **Flap `ordinal` ready again** — the counterpart of
    /// [`flap_not_ready`](Self::flap_not_ready): the endpoint reads `ready=true`
    /// at the same address. The proxy publishes it as a fresh `Unknown` endpoint
    /// (nothing has probed it yet); every other live worker's link is a plain
    /// active peer again, with no puller respawn (ADR-0031 D1).
    pub fn flap_ready(&mut self, ordinal: &str) {
        self.mark(ordinal, None, "flap_ready", "endpoint ready again at the same address");
        self.views.record("orchestrator", ordinal, Belief::Admitted, "flap_ready");
        self.set_member_ready(ordinal, true, false, "flap_ready");
    }

    /// Drive one endpoint-condition change into the proxy's registry and every
    /// live peer's membership, recording what each observer is expected to hold:
    /// the proxy departs a not-ready member and publishes a ready one `Unknown`;
    /// a peer keeps a not-ready member and treats a ready one as plain active.
    fn set_member_ready(&mut self, ordinal: &str, ready: bool, terminating: bool, signal: &str) {
        if let Some(registry) = self.views.proxy_registry() {
            registry.set_ready(ordinal, ready);
            let belief =
                if ready { Belief::Registered(WorkerHealth::Unknown) } else { Belief::Absent };
            self.views.record("proxy", ordinal, belief, signal);
        }
        let belief = if ready { Belief::PeerActive } else { Belief::PeerKept };
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.set_conditions(ordinal, ready, terminating);
            self.views.record(&observer, ordinal, belief, signal);
        }
        // The informer shows a worker its own endpoint too: a condition flip
        // reaches the member itself, which is how it observes its own
        // withdrawal (ADR-0031 D6).
        for (_, membership) in self.views.live_incarnations_of(ordinal) {
            membership.set_conditions(ordinal, ready, terminating);
        }
    }

    /// **Depart `ordinal`: its endpoint leaves the slice** — the pod is gone
    /// (ADR-0031 case 1, the instant after the drain). The proxy drops the
    /// ordinal (it was already unroutable under
    /// [`withdraw_routing`](Self::withdraw_routing)) and every other live
    /// worker's membership drops the peer, so its supervisor parks the flows.
    /// The process, if still running, is untouched: pair with `crash` to model
    /// the exit.
    pub fn depart(&mut self, ordinal: &str) {
        self.mark(ordinal, None, "depart", "endpoint gone from the slice");
        self.views.record("orchestrator", ordinal, Belief::Departed, "depart");
        if let Some(registry) = self.views.proxy_registry() {
            registry.remove(ordinal);
            self.views.record("proxy", ordinal, Belief::Absent, "depart");
        }
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.remove(ordinal);
            self.views.record(&observer, ordinal, Belief::PeerParked, "depart");
        }
    }

    /// **Re-admit `ordinal` at `addr`** — the counterpart of
    /// [`withdraw`](Self::withdraw): the proxy publishes the endpoint again at
    /// `addr` with health `Unknown` (nothing has probed it yet) and every other
    /// live worker's membership re-adds the peer.
    pub fn readmit(&mut self, ordinal: &str, addr: SocketAddr) {
        self.mark(ordinal, None, "readmit", &format!("endpoint published at {addr}"));
        self.publish(ordinal, addr);
    }

    /// Publish `ordinal` at `addr` in the proxy's registry (`Unknown`) and in
    /// every live peer's membership.
    fn publish(&mut self, ordinal: &str, addr: SocketAddr) {
        self.views.record("orchestrator", ordinal, Belief::Admitted, "readmit");
        if let Some(registry) = self.views.proxy_registry() {
            registry.add(sip_proxy::registry::WorkerEntry {
                health: WorkerHealth::Unknown,
                ..sip_proxy::registry::WorkerEntry::alive(
                    ordinal,
                    ProxyAddr::new(addr.ip().to_string(), addr.port()),
                )
            });
            self.views.record(
                "proxy",
                ordinal,
                Belief::Registered(WorkerHealth::Unknown),
                "readmit",
            );
        }
        for (observer, membership) in self.views.live_peers_of(ordinal) {
            membership.add(Peer::new(ordinal, ordinal));
            self.views.record(&observer, ordinal, Belief::PeerActive, "readmit");
        }
        for (_, membership) in self.views.live_incarnations_of(ordinal) {
            membership.add(Peer::new(ordinal, ordinal));
        }
    }
}
