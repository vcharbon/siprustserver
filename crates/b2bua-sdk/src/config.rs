//! B2BUA runtime configuration — the subset the dispatcher / router / store /
//! rules read. Behavioural timeouts that have a
//! tokio analogue stay here as plain values.

use crate::sdp_form::{AsWritten, SdpFormPolicy};
use crate::service_params::ServiceParams;

mod bounds;
mod call_caps;
mod capacity;
mod cdr;
mod defaults;
mod event_queue_gate;
mod front;
mod retry_after;
mod validate;
pub use call_caps::{AgeCap, CallCaps, Cap, CapKind};
pub use capacity::{CapacityConfig, Ceilings};
pub use cdr::CdrConfig;
pub use event_queue_gate::{EventQueueGateConfig, GateBound, GateViolation, EVENT_QUEUE_CAPACITY};
pub use front::Front;
pub use retry_after::RetryAfterRange;

/// Tunables for a B2BUA worker. Cheap to clone (a handful of scalars + two
/// short strings); share one instance across the stack.
#[derive(Clone, Debug)]
pub struct B2buaConfig {
    /// What the process reads the wire under: stated once at startup, held by
    /// the transaction layer and the ingress brake alike ([`Self::parser`]).
    pub parser_limits: sip_message::SipParserLimits,
    /// This worker's ordinal, encoded into `callRef` for partition routing.
    pub self_ordinal: String,
    /// The key every worker of the cluster shares to sign what it states
    /// about its own requests (the via-extension, ADR-0055 §12): a peer
    /// verifies the statement on a response its request drew. `None`: the
    /// worker states nothing and trusts no statement.
    pub cluster_key: Option<sip_ids::SigningKey>,
    /// Local signaling IP stamped into Via, and into Contact standalone.
    pub sip_local_ip: String,
    /// Local signaling port stamped into Via, and into Contact standalone.
    pub sip_local_port: u16,
    /// The deployment mode: standalone, or behind the SIP front proxy.
    pub front: Front,
    /// Global cap on concurrently-running handlers across all calls.
    pub event_dispatch_concurrency: usize,
    /// Per-call queue depth (events buffered behind a busy handler).
    pub per_call_queue_depth: usize,
    /// Max number of live per-call queues (memory bound).
    pub per_call_queue_cap: usize,
    /// The share of `event_dispatch_concurrency`, in percent, that turns of
    /// normal initial INVITEs may hold at once (1..=100, default 50). Such a
    /// turn holds its permit across the decision round trip. An emergency
    /// INVITE draws from the whole pool, so established calls keep the rest
    /// of it less what emergency turns hold. See
    /// [`new_call_permits`](Self::new_call_permits).
    pub new_call_permit_share_percent: u8,
    /// The share of `per_call_queue_cap`, in percent, kept back from normal
    /// new INVITEs (0..=99, default 5): such an INVITE opens a call's queue
    /// only below the cap less this headroom, while an emergency INVITE and
    /// every in-dialog event (the first one of a taken-over call included)
    /// open one up to the full cap. See
    /// [`new_call_queue_headroom`](Self::new_call_queue_headroom).
    pub new_call_queue_headroom_percent: u8,
    /// Auto-terminate a call whose in-dialog event count exceeds this WITHIN
    /// one keepalive interval (loop guard). The keepalive tick resets the
    /// counter, so the budget is a rate: a runaway dialog lands >cap events
    /// inside one window and is torn down, while a healthy call outlasting any
    /// number of intervals never consumes the defense.
    pub max_messages_per_call: u64,
    /// The work bound on a call over its whole life, in every state: past
    /// this many events offered for it — counted at dispatch, before any
    /// queue bound, so a stuck handler or a full queue hides none — the call
    /// ends (ADR-0020). The node's own timers and internal events do not
    /// count. A healthy call stays far below it: a 24 h call with 300 s
    /// keepalives and session refreshes offers a few thousand. Enforced and
    /// checked (never 0) with the reaper's sweep on or off.
    ///
    /// A call held just under `max_messages_per_call` per keepalive window
    /// reaches it after `lifetime / max_messages_per_call × keepalive
    /// interval`: 500 windows, about 41.7 h, at the defaults.
    pub max_messages_per_call_lifetime: u64,
    /// Bounded CDR submit queue; `0` disables buffering (passthrough).
    pub cdr_buffer_queue_max: usize,
    /// REFER implicit-subscription expiry (RFC 3515), seconds. Armed at REFER
    /// intercept; fires while still `refer-authorizing` (HTTP hung). Default 60.
    pub refer_subscription_expiry_sec: i64,
    /// Per re-INVITE answer watchdog during REFER realignment, seconds. Default 32.
    pub refer_reinvite_answer_sec: i64,
    /// Overall REFER safety timer covering the whole transfer FSM, seconds. Default 120.
    pub refer_overall_safety_sec: i64,
    /// Overall safety timer covering the whole **established-call reroute**
    /// (a `Route`-shaped `call_release` decision): replacement
    /// b-leg dial + a-leg re-INVITE realign + old-leg BYE. Armed when the
    /// reroute is applied, cancelled on completion; on expiry the call is torn
    /// down (the release event stands — a wedged reroute must never extend the
    /// call past its cap). Default 120, mirroring `refer_overall_safety_sec`
    /// (the same two-re-INVITE realign shape).
    pub release_reroute_guard_sec: i64,
    /// In-dialog OPTIONS keepalive interval, seconds. The B2BUA arms a keepalive
    /// timer at dialog confirmation and re-arms it each cycle; on expiry it pokes
    /// every peered leg with an in-dialog OPTIONS. Production default is **300 s**
    /// (operator: "in-call OPTIONS every 5 minutes"); a shorter interval (e.g.
    /// 30 s) breaks long-hold endurance traffic by poking mid-dialog calls whose
    /// UAC is not expecting it. Overridable per worker via `B2BUA_KEEPALIVE_SEC`.
    /// The test harness lowers this to 30 s so paused-clock tests stay fast.
    /// A probe's only deadline is its own transaction's Timer F (32 s, RFC 3261
    /// §17.1.2.2): unanswered by then, it ends the call.
    pub keepalive_interval_sec: i64,
    /// Limiter-refresh cadence, seconds — how often a counted call extends its
    /// lease on the limiter. The lease is the limiter's own, stated in its
    /// answers; this period plus one refresh tick must stay below it by a
    /// margin that survives a missed refresh (the worker counts a learnt lease
    /// that it reaches): default 40 under the limiter's default 120. The test
    /// harness lowers this for fast paused-clock tests.
    pub limiter_refresh_sec: i64,
    /// Most releases the worker's limiter release queue holds; at the cap the
    /// oldest entry is dropped (its lease frees the call). Default 100 000:
    /// one lease of calls ending at 800/s.
    pub limiter_release_queue_cap: usize,
    /// The worker's limiter refresh tick, milliseconds: a refresh falling due
    /// is sent within one tick, with every other key due, in one request per
    /// [`limiter_refresh_batch_max`](Self::limiter_refresh_batch_max) keys.
    /// Below the refresh period; cut to a sixth of the lease when longer. Default 1000.
    pub limiter_refresh_batch_ms: u64,
    /// Most keys one limiter refresh request carries. Default 1000.
    pub limiter_refresh_batch_max: usize,
    /// **Call reaper sweep** switch (ADR-0020 X1). ON by default; `false` is a
    /// debugging escape hatch only — the in-process "released exactly once, one
    /// CDR" promise does not hold without it. Failure reports, trips and the
    /// lifetime cap's verdict act either way (ADR-0057 §4).
    pub reaper_enabled: bool,
    /// Check the call model's invariants at the end of every turn, before
    /// the record lands, and panic with the call's dump on a violation
    /// (`b2bua::model_invariants`). On by default in debug builds, off
    /// in release builds; every test harness turns it on.
    pub model_invariants: bool,
    /// Reaper sweep cadence, seconds (rides `tokio::time::interval` —
    /// deterministic under `start_paused` tests). Default 30 s; the sweep is
    /// one short store-lock pass, off the call path.
    pub reaper_sweep_interval_sec: i64,
    /// A live call whose **last-touched stamp** is older than this is
    /// reap-eligible (ADR-0020 X4). `0` (the default) derives
    /// `3 × keepalive_interval_sec` at spawn: every healthy call — even an idle
    /// long-hold — receives its keepalive OPTIONS `200` each interval (only
    /// **real SIP traffic** stamps the ledger; self-generated housekeeping
    /// turns like `LimiterRefresh` do not), so a stamp older than 3 intervals
    /// provably means a SIP-dead call, never quietness. The derived window is
    /// additionally floored above
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) — a ringing
    /// call stamps nothing until its setup resolves. An explicit value must be
    /// ≥ `2 × keepalive_interval_sec` AND > the transaction bound (enforced by
    /// [`validate`](Self::validate)). Liveness derives ONLY from the stamp —
    /// never `created_at`, never timer deadlines.
    pub reaper_idle_max_sec: i64,
    /// **Setup timeout**, milliseconds — the caller's initial-INVITE deadline:
    /// armed at route time, cancelled at answer, deliberately NOT reset
    /// by reroute/failover (each new b-leg that rings gets its own `NoAnswer`;
    /// this caps the caller's *total* wait for a final response). It rides the
    /// call record's timer ledger, which outlives the transactions that bound
    /// ([`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec)) lives in, so
    /// a call whose transactions are gone is still ended at the deadline rather
    /// than holding its limiter slots for the full 1 h GlobalDuration.
    /// Default **150 000 ms**: strictly below the configured transaction bound
    /// (enforced by [`validate`](Self::validate)) so the rules path owns the
    /// teardown (408 to the caller, CANCEL to pending b-legs, obligations
    /// settled), above any sane no-answer timeout so a route-supplied `NoAnswer`
    /// (run from the leg's first 18x) normally fires first. `<= 0` disables (the txn backstop and GlobalDuration
    /// remain). Overridable via `B2BUA_SETUP_TIMEOUT_MS`.
    pub setup_timeout_ms: i64,
    /// **Initial-INVITE transaction bound**, seconds — the sip-txn
    /// out-of-dialog INVITE give-up window
    /// (`TransactionConfig::invite_initial_timeout_ms`), bounding BOTH halves
    /// of a call: the b-leg client txn's give-up AND the a-leg server txn's
    /// pre-final backstop derive from it. The last-resort backstop under every
    /// app-level setup deadline: [`validate`](Self::validate) requires
    /// `setup_timeout_ms < invite_txn_timeout_sec × 1000` (when enabled) and
    /// range-checks [`MIN_INVITE_TXN_TIMEOUT_SEC`](Self::MIN_INVITE_TXN_TIMEOUT_SEC)
    /// `..=` [`MAX_INVITE_TXN_TIMEOUT_SEC`](Self::MAX_INVITE_TXN_TIMEOUT_SEC);
    /// a route-supplied `NoAnswer` falling later than
    /// `bound − `[`NO_ANSWER_CANCEL_MARGIN_SEC`](Self::NO_ANSWER_CANCEL_MARGIN_SEC)
    /// after the leg's INVITE is held to that instant.
    /// Default [`DEFAULT_INVITE_TXN_TIMEOUT_SEC`](Self::DEFAULT_INVITE_TXN_TIMEOUT_SEC)
    /// (158 s); telephony deployments (Timer C > 3 min, 180 s PSTN
    /// supervision, hunting chains) raise it. Overridable via
    /// `B2BUA_INVITE_TXN_TIMEOUT_SEC`.
    pub invite_txn_timeout_sec: i64,
    /// **Initial-INVITE first-response deadline**, seconds — how long this TU
    /// waits for a response of ANY kind (not even a `100 Trying`) to a b-leg
    /// initial INVITE before it gives up on the hop (ADR-0044): the leg fails
    /// with a `TxnTimeoutKind::Response` and the `call_failure` consult carries
    /// `timeout_kind: "response"` — the "is this hop alive?" question, distinct
    /// from [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec)'s "how
    /// long may it ring?". The client transaction keeps Timer A to Timer B.
    /// Only the initial INVITE reads it — an in-dialog INVITE and a
    /// non-INVITE never do.
    /// [`validate`](Self::validate) range-checks
    /// [`MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC`](Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC)
    /// `..=` [`MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC`](Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC).
    /// Default **5 s**: a silent next hop is given up fast. Overridable via
    /// `B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC`.
    pub invite_first_response_timeout_sec: i64,
    /// **Un-ACKed 2xx give-up deadline**, seconds (RFC 3261 §13.3.1.4; the
    /// RFC's `64·T1` = 32 s is the default). When it elapses with no ACK for a
    /// 2xx to an INVITE — initial or re-INVITE — the peer is gone and the
    /// session ends: the B2BUA BYEs the a-leg dialog and the b-leg it bridges.
    /// Only the deadline is configured: the 2xx ladder always runs to Timer L
    /// and the teardown always acts (ADR-0032 X5). `<= 0` is NOT allowed —
    /// [`validate`](Self::validate) refuses it, and a harness config that
    /// writes it anyway gets the default
    /// ([`ack_timeout_ms`](Self::ack_timeout_ms)), as
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) does.
    /// Overridable via `B2BUA_ACK_TIMEOUT_SEC`.
    pub ack_timeout_sec: i64,
    /// **CPS bucket rung capacity**. The hard ceiling on a *burst* of new-dialog
    /// INVITEs this worker will admit: tokens accrue at
    /// [`cps_bucket_rate`](Self::cps_bucket_rate)/s up to this cap, and the
    /// admission gate consumes one per new INVITE (an emergency caller is
    /// admitted on an empty bucket and owes nothing). `0` disables the
    /// hard CPS gate (every non-emergency INVITE passes the bucket — the
    /// panic-ELU backstop still applies). Default **1000**. Overridable via
    /// `B2BUA_CPS_BUCKET_SIZE`.
    pub cps_bucket_size: u32,
    /// **CPS bucket rung refill rate** (tokens/sec). The sustained new-INVITE rate the bucket
    /// permits once its burst capacity is drained. Default **500**.
    /// Overridable via `B2BUA_CPS_BUCKET_RATE`.
    pub cps_bucket_rate: u32,
    /// **Panic-ELU rung threshold** (`0..1`).
    /// A *backstop* on the worker's OWN EWMA-smoothed Event-Loop Utilization:
    /// above it, a non-emergency new INVITE that already passed the CPS bucket
    /// is still 503'd locally, regardless of the LB's AIMD cap. The LB-side AIMD
    /// (`sip_proxy::load_observer`) is the primary control loop; this catches the
    /// cases where the LB is absent, misconfigured, or itself overloaded. Kept
    /// high so it almost never fires in normal operation. Default **0.75**.
    /// `>= 1.0` effectively disables it (the clamped ELU never exceeds 1).
    /// Overridable via `B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD`.
    pub overload_panic_elu_threshold: f64,
    /// **Retry-After base** (seconds) of
    /// every new-call 503: the panic-ELU and capacity rejects, the ingress
    /// brake, the deferred backlog and the dispatcher cap shed. The
    /// `bucket_empty` 503 starts from the later of this and the bucket's
    /// time-to-next-token. A new-call reject never carries less than 1 s,
    /// whatever this value. Default **5**.
    /// Overridable via `B2BUA_RETRY_AFTER_BASE_SEC`.
    pub retry_after_base_sec: u32,
    /// **Retry-After jitter span** (seconds) of every new-call 503 (ADR-0037
    /// item 3): uniform over `[b, b + jitter]` with `b` the base above floored
    /// at 1 s, so callers refused together do not return together. `0` pins it
    /// to `b`.
    /// Overridable via `B2BUA_RETRY_AFTER_JITTER_SEC`.
    pub retry_after_jitter_sec: u32,
    /// The `Retry-After` range a `491 Request Pending` carries when it refuses
    /// an INVITE whose sender's earlier INVITE has its 2xx from this side and
    /// has not ACKed it yet (RFC 3261 §20.33 leaves the header optional there).
    /// `None`, the default: no `Retry-After` on any 491.
    pub glare_retry_after: Option<RetryAfterRange>,
    /// **b-leg destination allow-list**. The decision boundary classifies
    /// `route.destination.host` against this list (see `destination_allowlist`):
    /// an IP literal always passes; otherwise the host must end with one of these
    /// suffixes (case-insensitive), else the gate emits a `503` and terminates
    /// the call *before* any b-leg state is allocated. This stops a bogus host (a
    /// typo, a `.svc.cluster.local` name the K8s runner constructs, a dev
    /// `/etc/hosts` entry) from reaching the send path and blocking on
    /// `getaddrinfo`/`EAI_AGAIN`. The literal `"*"` matches every host (a
    /// rollback sentinel — restores admit-everything behaviour without a
    /// redeploy). Default `".svc.cluster.local"` (the K8s in-cluster DNS suffix);
    /// an empty list rejects every non-IP host. Overridable via
    /// `WORKER_ALLOWED_TARGET_SUFFIXES` (comma-separated, trimmed, empties
    /// dropped).
    pub worker_allowed_target_suffixes: Vec<String>,
    /// **The setup budget and the decision-backend deadline** (ms). On an
    /// initial INVITE's turn: the bound from the INVITE's receipt to its final,
    /// the wait for a handler permit, the decision round trips and the limiter's
    /// admits together, then the bare 503 (ADR-0022 X1,
    /// `b2bua::setup_budget`). Off the turn: the bound on one round trip of a
    /// consult (`DeadlineDecisionEngine`; `call_refer` has the refer timers
    /// instead). The core owns this as ONE knob, not each adapter, so a
    /// third-party adapter cannot forget it. Default **5000**. `<= 0` is NOT
    /// allowed: [`validate`](Self::validate) refuses it, and a harness config
    /// that writes it anyway gets the default
    /// ([`call_control_timeout`](Self::call_control_timeout), the one reader).
    /// Overridable via `B2BUA_CALL_CONTROL_TIMEOUT_MS`.
    pub call_control_timeout_ms: i64,
    /// **Opt-in transparent header relay.** Header names whose *value* is copied
    /// verbatim from the a-leg INVITE onto every originated b-leg INVITE (the
    /// normal callee leg AND the REFER transfer-target leg — both funnel through
    /// the single `build_b_leg` mint point). Names are matched case-insensitively.
    /// **Empty = no relay**, which is the production default and a strict no-op.
    /// Structural headers the generator owns (Via/From/To/Contact/Call-ID/CSeq/
    /// Max-Forwards/Route/Record-Route/Content-Length/Content-Type) are NEVER
    /// relayable even if named here, so a misconfiguration cannot corrupt the
    /// dialog. Overridable per worker via `B2BUA_RELAY_HEADERS` (comma-separated).
    pub relay_headers: Vec<String>,
    /// **Privacy service** (RFC 3323 §5, RFC 3325 §7). `true` (the default):
    /// this worker is the trust boundary, so a relayed message whose `Privacy`
    /// conceals the identity leaves the asserted identity behind. `false`: the
    /// next hop is the boundary, and the asserted identity rides beside the
    /// privacy request. Overridable via `B2BUA_PRIVACY_SERVICE`.
    pub privacy_service: bool,
    /// **Relay policy.** The headers a relayed message leaves behind, by
    /// header, message and direction ([`RelayPolicy`]), consulted at every
    /// relay site except the originated INVITE, whose headers the routing
    /// decision states. Transparent by default; a deployment sets it in code.
    ///
    /// [`RelayPolicy`]: sip_message::generators::RelayPolicy
    pub relay_policy: sip_message::generators::RelayPolicy,
    /// How a CANCEL this stack mints toward a pending leg restates the
    /// canceller's `Reason` (RFC 3326 §2): verbatim by default.
    pub relayed_cancel_reason: crate::release_reason::CancelReason,
    /// The `Reason` (RFC 3326) on every BYE of a release the stack makes on
    /// its own, never one a peer's BYE or CANCEL asked for, where no other
    /// `Reason` is stated. `None` (the default) states none.
    pub own_release_reason: Option<String>,
    /// **Default SDP source** (a service parameter). A canned SDP body a service
    /// rule can source (via `ctx.config`) to originate a deliberate *fake-offer*
    /// INVITE — e.g. a delayed-offer flow where the service sends `INVITE(SDP)` it
    /// authored itself. A `CreateLeg` opts in explicitly with
    /// `body_override: ctx.config.default_sdp.clone()`; it is **never** an
    /// automatic fallback (a normal reroute/failover `CreateLeg` passes
    /// `body_override: None` on purpose to relay the caller's own offer). `None`
    /// (the default) = no canned SDP.
    pub default_sdp: Option<Vec<u8>>,
    /// The service's hook deciding the form an in-dialog session description
    /// leaves in ([`SdpFormPolicy`]); [`AsWritten`] by default.
    pub sdp_form: std::sync::Arc<dyn SdpFormPolicy>,
    /// **Node capability advertisement.** The `Allow`/`Supported`/`Accept`
    /// set this worker states on every 200 it sends to an OPTIONS (RFC 3261
    /// §11.2) — the out-of-dialog health reply, the one it answers in a
    /// dialog, the one it relays — a node-scoped statement, not a call-scoped
    /// one, so it is declared here rather than by a routing decision. The
    /// default is the stack set; the health path borrows this value, so a
    /// keepalive probe resolves its advertisement without allocating.
    pub node_capabilities: sip_message::generators::CapabilitySet,
    /// **Minted-final advertisement.** The `Allow`/`Supported`/`Accept` this
    /// worker states on a failure final to an INVITE it mints in its own name,
    /// restating no peer's final (a reject, a refusal, a timeout's final; not
    /// 487). Silent by default; a deployment sets it in code.
    pub minted_final_advertisement: sip_message::generators::CapabilitySet,
    /// The call record's raw context — the per-leg message ring and the
    /// headers it captures. Off by default: the runner turns it on.
    pub cdr: CdrConfig,
    /// **Capacity admission** (ADR-0037): the live-call, transaction and RSS
    /// ceilings above which a new call is refused. Every bound is off by
    /// default.
    pub capacity: CapacityConfig,
    /// **Event-queue gate** (ADR-0050 §9): the depth and wait of the
    /// transaction event queue at which a `Ready` worker refuses new calls on
    /// its OPTIONS answer, and the bounds and hold under which it accepts
    /// them again.
    pub event_queue_gate: EventQueueGateConfig,
    /// **Service parameters.** Product services carry their own parameters
    /// here, one value per type; their rules read them through `ctx.config`.
    /// Empty by default.
    pub service_params: ServiceParams,
    /// **Per-call caps** (ADR-0057): what one call's record may hold and what
    /// one turn may do; a crossing fails the turn. The upstream defaults are
    /// [`CallCaps::DEFAULT`].
    pub caps: CallCaps,
}

impl B2buaConfig {
    /// The longest a service's HTTP request runs ([`CallCaps::service_http_timeout_ms`]).
    pub fn service_http_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.caps.service_http_timeout_ms)
    }

    /// The reader of the wire, whichever stage reads a datagram first.
    pub fn parser(&self) -> sip_message::CustomParser {
        self.front.parser(&self.parser_limits)
    }

    /// The configured initial-INVITE transaction bound in ms — the value wired
    /// into `TransactionConfig::invite_initial_timeout_ms`. `<= 0` is NOT
    /// "disabled" (unlike [`setup_timeout_ms`](Self::setup_timeout_ms)):
    /// a non-positive value falls back to
    /// [`DEFAULT_INVITE_TXN_TIMEOUT_SEC`](Self::DEFAULT_INVITE_TXN_TIMEOUT_SEC)
    /// rather than arming a degenerate bound.
    pub fn invite_txn_timeout_ms(&self) -> u64 {
        let sec = if self.invite_txn_timeout_sec > 0 {
            self.invite_txn_timeout_sec
        } else {
            Self::DEFAULT_INVITE_TXN_TIMEOUT_SEC
        };
        sec.unsigned_abs().saturating_mul(1000)
    }

    /// The configured initial-INVITE first-response deadline in ms — the value
    /// wired into `TransactionConfig::invite_first_response_timeout_ms`. `<= 0`
    /// is NOT "disabled": a non-positive value falls back to the 5 s default
    /// rather than arming a degenerate deadline.
    pub fn invite_first_response_timeout_ms(&self) -> u64 {
        let sec = if self.invite_first_response_timeout_sec > 0 {
            self.invite_first_response_timeout_sec
        } else {
            Self::DEFAULT_INVITE_FIRST_RESPONSE_TIMEOUT_SEC
        };
        sec.unsigned_abs().saturating_mul(1000)
    }

    /// The privacy-service role every relay site mints under
    /// ([`privacy_service`](Self::privacy_service)).
    pub fn identity_privacy(&self) -> sip_message::generators::IdentityPrivacy {
        use sip_message::generators::IdentityPrivacy;
        if self.privacy_service {
            IdentityPrivacy::Conceal
        } else {
            IdentityPrivacy::Relay
        }
    }

    /// `base` as a relay site mints it in this deployment: under the
    /// privacy-service role ([`identity_privacy`](Self::identity_privacy)) and
    /// the [`relay_policy`](Self::relay_policy) read for `situation`.
    pub fn relay_scope<'a>(
        &'a self,
        base: sip_message::generators::RelayScope<'static>,
        situation: sip_message::generators::RelaySituation<'a>,
    ) -> sip_message::generators::RelayScope<'a> {
        base.with_identity(self.identity_privacy()).under(&self.relay_policy, situation)
    }

    /// The un-ACKed 2xx give-up deadline in ms — what arms a 2xx ladder's
    /// give-up. `<= 0` is NOT "disabled" (unlike
    /// [`setup_timeout_ms`](Self::setup_timeout_ms)): a non-positive value
    /// falls back to the 32 s default rather than leaving a 2xx's silence
    /// unanswered (RFC 3261 §13.3.1.4).
    pub fn ack_timeout_ms(&self) -> u64 {
        let sec = if self.ack_timeout_sec > 0 {
            self.ack_timeout_sec
        } else {
            Self::DEFAULT_ACK_TIMEOUT_SEC
        };
        u64::try_from(sec).unwrap_or(Self::DEFAULT_ACK_TIMEOUT_SEC as u64).saturating_mul(1000)
    }

    /// The setup budget and the decision deadline
    /// ([`call_control_timeout_ms`](Self::call_control_timeout_ms)), never
    /// zero: a non-positive value reads the 5 s default, so no consult runs
    /// unbounded.
    pub fn call_control_timeout(&self) -> std::time::Duration {
        Self::call_control_timeout_of(self.call_control_timeout_ms)
    }

    /// [`call_control_timeout`](Self::call_control_timeout) of a stated `ms`,
    /// for a reader that runs before the configuration holds it.
    pub fn call_control_timeout_of(ms: i64) -> std::time::Duration {
        let ms = if ms > 0 { ms } else { Self::DEFAULT_CALL_CONTROL_TIMEOUT_MS };
        std::time::Duration::from_millis(ms.unsigned_abs())
    }

    /// Clamp a route-supplied `NoAnswer` deadline, as ms after the leg's
    /// INVITE left, under the transaction bound: any value above `bound − `[`NO_ANSWER_CANCEL_MARGIN_SEC`](Self::NO_ANSWER_CANCEL_MARGIN_SEC)
    /// — including one merely inside the margin band, which would equally
    /// strand the CANCEL→487 exchange — is held to that ceiling so the
    /// exchange completes inside the live b-leg client transaction. Values at
    /// or under the ceiling pass through unchanged; the caller compares the
    /// result to detect (and `debug!`-note) a clamp.
    pub fn clamp_no_answer_ms(&self, requested_ms: i64) -> i64 {
        let ceiling = self.invite_txn_timeout_sec - Self::NO_ANSWER_CANCEL_MARGIN_SEC;
        requested_ms.min(ceiling.saturating_mul(1000))
    }

    /// The handler permits turns of normal initial INVITEs may hold at once:
    /// [`new_call_permit_share_percent`](Self::new_call_permit_share_percent)
    /// of `event_dispatch_concurrency`, rounded down, at least one.
    pub fn new_call_permits(&self) -> usize {
        let share = usize::from(self.new_call_permit_share_percent);
        (self.event_dispatch_concurrency.saturating_mul(share) / 100).max(1)
    }

    /// The live per-call queues kept back from normal new INVITEs:
    /// [`new_call_queue_headroom_percent`](Self::new_call_queue_headroom_percent)
    /// of `per_call_queue_cap`, rounded down.
    pub fn new_call_queue_headroom(&self) -> usize {
        let share = usize::from(self.new_call_queue_headroom_percent);
        self.per_call_queue_cap.saturating_mul(share) / 100
    }

    /// The effective reaper idle threshold, ms (ADR-0020 X4): the explicit
    /// override, or the derived `3 × keepalive_interval_sec` floored above
    /// [`invite_txn_timeout_sec`](Self::invite_txn_timeout_sec) — a
    /// legitimately-ringing call stamps no ledger liveness until its setup
    /// resolves, so the idle window must outlast the longest configured ring.
    pub fn reaper_idle_max_ms(&self) -> i64 {
        let sec = if self.reaper_idle_max_sec > 0 {
            self.reaper_idle_max_sec
        } else {
            (3 * self.keepalive_interval_sec)
                .max(self.invite_txn_timeout_sec + self.keepalive_interval_sec)
        };
        sec.saturating_mul(1000)
    }
}

#[cfg(test)]
mod tests;
