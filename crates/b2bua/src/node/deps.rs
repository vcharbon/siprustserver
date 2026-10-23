//! What a host hands [`B2buaCore`](super::B2buaCore) to spawn it: the wiring
//! inputs and the optional replication setup.

use std::sync::Arc;

use repl_net::transport::{ReplicationListener, ReplicationNetwork};
use sip_clock::Clock;
use sip_txn::{DeferredBound, IdGen};
use topology::Membership;

use crate::admission::Refusals;
use crate::capacity::CapacityGate;
use crate::cdr::CdrWriter;
use crate::config::B2buaConfig;
use crate::decision::CallDecisionEngine;
use crate::limiter::CallLimiter;
use crate::metrics::B2buaMetrics;
use crate::repl::ReplicatingCallStore;
use crate::store::{CallStore, StoreFaults};
use crate::turn_probe::TurnProbes;
use crate::wire_faults::WireFaults;

/// Optional replication wiring for [`B2buaDeps`]. Supplying `Some(..)` turns a
/// `B2buaCore` into a replicating worker; `None` is a non-replicating worker
/// (in-memory store, `always_ready()` OPTIONS, `PutOpts::default()` flush).
///
/// ## Host-supplied seams
/// - **`incarnation_gen`** — the per-boot incarnation seed for the
///   [`ReplicatingCallStore`]'s changelog (mirrors `IdGen::seeded`), an explicit
///   input the host derives (the runner uses the boot wall clock).
/// - **`addr_resolver`** — maps a cluster `Peer` to its replication socket
///   address, **resolved per connect** (ADR-0012 D3). The sim harness passes
///   an explicit `ordinal → addr` map (`FnPeerResolver`); the runner derives
///   it from `ordinal + host + config`. The core defines no addressing
///   grammar — the resolver IS the seam.
pub struct ReplicationSetup {
    /// The replication transport (sim or real). The server `listen`s on it and
    /// the supervisor's pullers `connect` through it.
    pub network: Arc<dyn ReplicationNetwork>,
    /// Cluster membership (who to replicate to/from).
    pub membership: Arc<dyn Membership>,
    /// The replicating call store (built with `incarnation_gen`). Used as the
    /// `CallState` store AND served to pulling peers.
    pub store: Arc<ReplicatingCallStore>,
    /// The listener this node serves its changelog on, bound by the host before
    /// the core spawns so that a bind failure refuses the boot.
    pub listener: Box<dyn ReplicationListener>,
    /// Resolves a peer to its replication address (the host's addressing seam).
    pub addr_resolver: crate::repl::AddrResolver,
    /// Per-boot incarnation seed for the changelog (host-derived).
    pub incarnation_gen: u64,
    /// The pullers' backoff, bootstrap and link bounds (ADR-0045).
    pub puller: crate::repl::PullerConfig,
    /// The serve loop's link bounds (ADR-0045).
    pub serve: crate::repl::ServeConfig,
    /// The replicas' TTL, the backup ceilings and the keepalive catch-up.
    pub replica: super::replica::ReplicaConfig,
}

/// Wiring inputs for [`B2buaCore::spawn`](super::B2buaCore::spawn).
pub struct B2buaDeps {
    pub config: B2buaConfig,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    pub cdr: Arc<dyn CdrWriter>,
    pub store: Arc<dyn CallStore>,
    /// Live-path store-fault probe (ADR-0023). Default = **no faults**: the
    /// router's live lookup sites (initial-INVITE dialog-existence check,
    /// in-dialog request fetch, keepalive/audit read) consult it before their
    /// sync map reads, and read the map unchanged while it is disarmed. A test
    /// retains a clone of the handle and flips per-path switches mid-call.
    /// Sits ONLY on the live-serving sites — the HA reclaim/reconcile/
    /// terminate-writer paths are deliberately un-probed.
    pub store_faults: StoreFaults,
    /// Wire-fault seam: one named RFC deviation the core emits on purpose so
    /// the post-run audit gate can be proven live. Default = never armed.
    pub wire_faults: WireFaults,
    /// Turn-point seam (ADR-0055 §13): a test observes and may hold each turn
    /// of a call at its sub-points. Default = no probe.
    pub turn_probe: TurnProbes,
    pub clock: Clock,
    pub id_gen: Arc<IdGen>,
    /// The worker's refusals of new INVITEs ([`crate::admission::Refusals`]),
    /// shared with the transaction layer. A host that installs the ingress
    /// brake passes the same instance it gave the brake, so every copy of one
    /// INVITE draws one answer. `None` builds them from `config`'s
    /// `Retry-After`, keyed by an entropy-drawn secret.
    pub refusals: Option<Refusals>,
    /// The deferred backlog's ceilings. `None` — production — sets one and two
    /// event queues' worth ([`crate::admission::deferred_bound`]); a test sets
    /// its own ([`crate::admission::ceilings`]) to
    /// reach the backlog refusal without stalling the router.
    pub deferred_ceilings: Option<DeferredBound>,
    /// Opt-in replication. `None` → a non-replicating worker.
    pub replication: Option<ReplicationSetup>,
    /// Shared metrics handle. The host builds this so components it constructs
    /// *before* spawn (notably the CDR writers) record into the SAME registry the
    /// core exports at `/metrics`. Pass `B2buaMetrics::new()` if you don't scrape it.
    pub metrics: B2buaMetrics,
    /// Host-injected generic async-HTTP capability for
    /// [`RuleAction::ServiceHttpRequest`](crate::rules::RuleAction) (the
    /// service-authorable adaptation callback). `None` = no capability (a
    /// service firing the effect gets an `outcome:"error"` re-entry, never a
    /// stranded machine); `Some` maps the logical endpoint onto its base URL.
    pub adaptation_http: Option<crate::router::AdaptationHttpPort>,
    /// Compose-time selection of which built-in CORE machines participate in the
    /// default rule set (ADR-0016 opt-out seam). `Default` =
    /// every built-in included. A downstream that ships
    /// its own subscription-gated transfer machine sets
    /// [`ComposeOptions::without_core_refer_transfer`](crate::rules::ComposeOptions::without_core_refer_transfer)
    /// so an in-dialog REFER relays transparently instead of being intercepted.
    pub compose: crate::rules::ComposeOptions,
    /// Memory admission gate (ADR-0037), configured here from
    /// `config.capacity`. `None` builds [`CapacityGate::live`]. A host that
    /// installs the gate on its ingress brake passes the same gate; a test
    /// passes one over a simulated [`SystemProbe`](crate::capacity::SystemProbe).
    pub capacity: Option<CapacityGate>,
}
