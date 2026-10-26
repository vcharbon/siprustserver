//! What varies between the b2bua spawns the harnesses share.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::cdr::CdrWriter;
use b2bua::config::Front;
use b2bua::decision::CallDecisionEngine;
use b2bua::limiter::CallLimiter;
use b2bua::store::{CallStore, StoreFaults};
use b2bua::wire_faults::WireFaults;
use b2bua::ReplicationSetup;
use sip_clock::Clock;
use sip_txn::IdGen;

/// Everything that varies between a single-SUT and a replicated b2bua spawn.
/// The caller binds its own endpoint and passes it (plus these params) to
/// [`spawn_b2bua_core`](crate::spawn_b2bua_core).
pub struct B2buaSpawnParams {
    /// `self_ordinal` (e.g. "w0" single-SUT, the worker ordinal in a cluster).
    pub ordinal: String,
    /// The worker's identity, one boot of its ordinal (ADR-0055 §6). Empty:
    /// it states none.
    pub identity: String,
    /// This worker's SIP wire address (drives `sip_local_ip`/`sip_local_port`).
    pub sip_addr: SocketAddr,
    pub decision: Arc<dyn CallDecisionEngine>,
    pub limiter: Arc<dyn CallLimiter>,
    /// Callflow services to register (ADR-0016). Empty for the plain path.
    pub services: Vec<b2bua::rules::ServiceDef>,
    /// The worker's deployment mode: standalone, or behind the proxy.
    pub front: Front,
    /// Already-built replication setup (`None` → non-replicating). The caller
    /// constructs this — keeping repl-net/topology deps out of this crate.
    pub replication: Option<ReplicationSetup>,
    pub clock: Clock,
    pub id_gen: Arc<IdGen>,
    /// CDR writer handed to the deps (the caller keeps its own handle to
    /// snapshot what it wrote).
    pub cdr: Arc<dyn CdrWriter>,
    /// Worker-side overload signal to inject. `None` → the core mints a fresh
    /// [`OverloadSignal::live`](b2bua::overload::OverloadSignal::live). A
    /// `start_paused` test passes one built over the `simulated()`
    /// sampler (retaining its control) so it can drive a known ELU THROUGH the
    /// running 100 ms sampler task into the published `X-Overload` header.
    pub overload: Option<b2bua::overload::OverloadSignal>,
    /// Host-injected generic async-HTTP capability for a service's
    /// `RuleAction::ServiceHttpRequest` (ADR-0016 seam). `None` → a service
    /// firing the effect gets an `outcome:"error"` re-entry. A test builds one
    /// over `SimulatedHttpNetwork` (mirroring the limiter injection) to
    /// round-trip a binary adaptation body.
    pub adaptation_http: Option<b2bua::AdaptationHttpPort>,
    /// Compose-time built-in-machine selection (ADR-0016 opt-out seam).
    /// `Default` = every built-in included. A downstream that
    /// owns REFER via its own transfer machine passes
    /// `ComposeOptions::default().without_core_refer_transfer()`.
    pub compose: b2bua::rules::ComposeOptions,
    /// [`CallStore`] override (ADR-0023). `None` → a fresh
    /// [`InMemoryCallStore`](b2bua::store::InMemoryCallStore). On the replicating path the replication store is
    /// the drain target and this slot is unused.
    pub store: Option<Arc<dyn CallStore>>,
    /// Store fault-injection handle (ADR-0023). `None` → no faults. `Some` does
    /// two things with the SAME handle: wraps the (default or overridden)
    /// store in a [`FaultInjectingCallStore`](b2bua::store::FaultInjectingCallStore) and threads the handle as the
    /// router's live-path probe, so one control drives both halves of the seam.
    pub store_faults: Option<StoreFaults>,
    /// Wire-fault handle (`b2bua::wire_faults`). `None` → compliant emission.
    pub wire_faults: Option<WireFaults>,
    /// Turn-point seam (`b2bua::turn_probe`, ADR-0055 §13). `None` → no probe.
    pub turn_probe: Option<b2bua::TurnProbes>,
    /// Memory admission gate (ADR-0037). `None` → the core builds one over the
    /// real process. A test passes one over a simulated
    /// [`SystemProbe`](b2bua::capacity::SystemProbe) to drive the RSS bounds.
    pub capacity: Option<b2bua::capacity::CapacityGate>,
    /// The deferred backlog's ceilings ([`B2buaDeps::deferred_ceilings`](b2bua::B2buaDeps::deferred_ceilings)).
    /// `None` → the worker's own.
    pub deferred_ceilings: Option<sip_txn::DeferredBound>,
    /// The node the worker serves ([`observe::NodeScope`]): what its log lines
    /// and per-call trace facts name in a failure dump. `None` → `ordinal`.
    pub node: Option<String>,
}
