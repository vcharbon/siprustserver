//! The fluent, dialog-aware harness — the auto-generating DSL (port of the
//! load-bearing half of `recorder.ts`'s `AgentProxy` / `DialogRef` + the dialog
//! state in `message-builder.ts`).
//!
//! This is the layer that means a scenario does **not** hand-author headers.
//! Agents are stateful UAs; each high-level call generates a correct-by-default
//! B2B message via `sip_message::generators` and tracks the dialog state needed
//! for the next one:
//!
//! ```ignore
//! let h = Harness::new("alice-calls-bob");
//! let alice = h.agent("alice", "127.0.0.1:5060").await;
//! let bob   = h.agent("bob",   "127.0.0.1:5070").await;
//!
//! let mut call = alice.invite(&bob).with_sdp(OFFER).send().await; // INVITE auto-built
//! let mut uas  = bob.receive("INVITE").await;
//! uas.respond(180, "Ringing").await;                              // To-tag minted here
//! call.expect(180).await;                                         // learns remote tag/target
//! uas.respond(200, "OK").with_sdp(ANSWER).await;
//! call.expect(200).await;
//! let mut dialog = call.ack().await;                              // ACK reuses INVITE CSeq
//! bob.receive("ACK").await;
//! let mut bye = dialog.bye().await;                               // BYE auto-increments CSeq
//! bob.receive("BYE").await.respond(200, "OK").await;
//! bye.expect(200).await;
//! let report = h.finish().await;                                  // render from the recording
//! ```
//!
//! What the harness fills in automatically, per RFC 3261: Via (fresh branch per
//! transaction, magic cookie), From/To with tags, Call-ID continuity, CSeq
//! numbering (1 INVITE → 1 ACK → n BYE; responses echo), Contact, Max-Forwards,
//! Content-Type/Length, remote-target routing (in-dialog requests go to the
//! peer's Contact). Everything still flows through the recording-wrapped
//! `SignalingNetwork`, so the reports are projected from the record exactly as
//! before — the auto-generation only changes *who writes the bytes*.

use std::cell::{Cell, RefCell};
use std::collections::hash_map::RandomState;
use std::collections::BTreeMap;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use layer_harness::{Channel, NetworkTag, Recorder, RunContext, TransportKind};
use sip_clock::Clock;
use sip_message::generators::{
    generate_ack_for_2xx, generate_ack_for_non_2xx, generate_cancel, generate_in_dialog_request,
    generate_out_of_dialog_request,
    generate_response, strip_route_uri_to_request_uri, ContactSpec, GenerateAckFor2xxOpts,
    GenerateInDialogRequestOpts, GenerateOutOfDialogRequestOpts, GenerateResponseOpts,
    InDialogMethod, InviteClientTransactionHandle, OutOfDialogMethod, SipTransport, StackDialog,
    ViaSpec, B2BUA_ALLOW, B2BUA_SUPPORTED,
};
use sip_message::message_helpers::{get_header, get_headers, set_header};

use crate::realcall::auth::{parse_challenge, ChallengeResponder};
use sip_message::parser::custom::CustomParser;
use sip_message::{serialize, SipHeader, SipMessage, SipParser, SipRequest, SipResponse};
use sip_net::{
    to_sip_entries, with_all_contracts, BindUdpOpts, ScopedAuditOptions, SignalingNetwork,
    SignalingNetworkEvent, SimulatedSignalingNetwork, UdpEndpoint,
};

use crate::report::wire::{facets, format_relative};
use crate::run::RunReport;

const RECV_TIMEOUT: Duration = Duration::from_secs(2);

/// Monotonic id source for branches / tags / Call-IDs. Deterministic (no RNG),
/// so report bytes are stable across runs. `pub(crate)` so the Send
/// [`loadbind::AgentBinder`] can share an id source the same way [`Harness`] does.
pub(crate) struct Ids(pub(crate) AtomicU64);
impl Ids {
    pub(crate) fn next(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed)
    }
}

/// A fallible step outcome — the error type of the agent's ONE receive/expect/
/// send core (newkahneed-034 ask C). Every primitive is implemented once,
/// fallibly (`try_receive`, `try_expect`, `try_send`, …); the panicking fluent
/// methods (`receive`/`expect`/`send`) are thin veneers that [`unwrap_step`]
/// the error into a `panic!` carrying this error's `Display`. Two policies,
/// one mechanism: a `#[tokio::test]` stops the world at the first divergence
/// (panic → [`PanicDump`] renders the wire trace), while the load driver
/// *counts* the failure and keeps going (see `loadgen::class`, which matches
/// on the VARIANT — the `Display` text is for humans).
#[derive(Debug, Clone)]
pub enum StepError {
    /// No datagram arrived within the agent's `recv_timeout`.
    Timeout { who: String },
    /// The endpoint's receive queue closed (socket/task gone).
    QueueClosed { who: String },
    /// A datagram arrived but did not parse as SIP.
    Unparseable { who: String, detail: String },
    /// A response arrived with the wrong status code.
    WrongStatus { who: String, expected: u16, got: u16, reason: String },
    /// A request arrived with the wrong method.
    WrongMethod { who: String, expected: String, got: String },
    /// A request arrived where a response was expected (or vice-versa).
    UnexpectedKind { who: String, detail: String },
    /// Sending a datagram failed at the transport.
    Transport { who: String, detail: String },
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepError::Timeout { who } => write!(f, "{who} timed out waiting for a datagram"),
            StepError::QueueClosed { who } => write!(f, "{who} endpoint queue closed"),
            StepError::Unparseable { who, detail } => {
                write!(f, "{who} received an unparseable datagram: {detail}")
            }
            StepError::WrongStatus { who, expected, got, reason } => {
                write!(f, "{who} expected {expected}, got {got} {reason}")
            }
            StepError::WrongMethod { who, expected, got } => {
                write!(f, "{who} expected a {expected} request, got {got}")
            }
            StepError::UnexpectedKind { who, detail } => write!(f, "{who}: {detail}"),
            StepError::Transport { who, detail } => write!(f, "{who} send failed: {detail}"),
        }
    }
}
impl std::error::Error for StepError {}

/// Unwrap a fallible-core step for the panicking (functional-test) lane: `Ok`
/// passes through, `Err` becomes `panic!("{e}")` — the same message the load
/// lane would have counted, now stopping the world instead (the [`PanicDump`]
/// guard renders the wire trace). `#[track_caller]` so the panic reports the
/// veneer's line; it cannot reach the *test's* line yet because
/// `#[track_caller]` on an `async fn` is still a no-op on stable
/// (rust-lang/rust#110011) — no loss, the panicking bodies always reported
/// `agent.rs` lines.
#[track_caller]
fn unwrap_step<T>(r: Result<T, StepError>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("{e}"),
    }
}

/// How a simulated UA, acting as UAS, echoes multiple Record-Route header rows
/// back in its response: as separate `Record-Route:` lines, or folded into a
/// single comma-separated list. RFC 3261 §7.3.1 permits the combined form, and
/// real UAs (SIPp, many phones) emit it — so the b-leg 200 OK can carry the front
/// proxy's *double*-record-route halves comma-combined in ONE header, which the
/// B2BUA must split before the §12.1.2 route-set reverse (the long-call-loss
/// class — see `b2bua/src/rules/actions.rs`). The harness picks this per-UA at
/// bind time so a run exercises both wire forms.
///
/// NOTE: this only has an observable effect when a response echoes ≥ 2
/// Record-Route headers, which in practice means the *real* double-record-routing
/// `sip-proxy` (failover-harness). The harness's own loose-routing [`Proxy`]
/// inserts a single `;lr` Record-Route, so folding is a no-op there and the
/// deterministic report bytes of peer-to-peer scenarios are unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordRouteFold {
    /// One `Record-Route:` line per route (the strict, current behaviour).
    Separate,
    /// All Record-Route rows folded into one comma-separated header (§7.3.1).
    Combined,
}

/// Process-wide random seed for the per-UA fold coin flip, drawn ONCE per launch
/// (so the two halves vary run-to-run) but shared by every UA (so a given name's
/// choice is stable within the run and reproducible from the logged line).
fn rr_fold_seed() -> &'static RandomState {
    static SEED: OnceLock<RandomState> = OnceLock::new();
    SEED.get_or_init(RandomState::new)
}

/// Decide a UA's Record-Route fold mode. `HARNESS_RR_FOLD=separate|combined`
/// pins it (for deterministic / repro runs); otherwise it is a per-UA coin flip
/// keyed on the UA name and the per-launch [`rr_fold_seed`].
pub(crate) fn decide_rr_fold(name: &str) -> RecordRouteFold {
    match std::env::var("HARNESS_RR_FOLD").ok().as_deref() {
        Some("separate") => RecordRouteFold::Separate,
        Some("combined") => RecordRouteFold::Combined,
        _ => {
            use std::hash::{BuildHasher, Hasher};
            let mut h = rr_fold_seed().build_hasher();
            h.write(name.as_bytes());
            if h.finish() & 1 == 0 {
                RecordRouteFold::Separate
            } else {
                RecordRouteFold::Combined
            }
        }
    }
}

/// Fold every Record-Route header in `headers` into a single comma-separated
/// header at the position of the first (RFC 3261 §7.3.1). No-op for < 2 rows.
fn fold_record_routes(headers: &mut Vec<SipHeader>) {
    let idxs: Vec<usize> = headers
        .iter()
        .enumerate()
        .filter(|(_, h)| h.name.eq_ignore_ascii_case("record-route"))
        .map(|(i, _)| i)
        .collect();
    if idxs.len() < 2 {
        return;
    }
    let combined = idxs
        .iter()
        .map(|&i| headers[i].value.clone())
        .collect::<Vec<_>>()
        .join(", ");
    headers[idxs[0]].value = combined;
    for &i in idxs[1..].iter().rev() {
        headers.remove(i);
    }
}

/// A running fluent session: owns the recording-wrapped simulated network and
/// hands out [`Agent`]s. Drop or [`finish`](Harness::finish) when done.
pub struct Harness {
    network: Arc<dyn SignalingNetwork>,
    recording: sip_net::RecordingSignalingNetwork,
    recorder: Recorder,
    ids: Arc<Ids>,
    name: String,
    description: Option<String>,
    /// Dumps the recorded trace to stderr if the scenario task unwinds before
    /// [`finish`](Harness::finish) renders a report. `finish` disarms it.
    dump: PanicDump,
    /// MANDATORY HARD GATE: fails the test on Drop if the recorded trace violates
    /// any non-advisory RFC rule — the backstop for a harness dropped WITHOUT
    /// [`finish`](Harness::finish) (which enforces the same gate inline).
    /// `finish` disarms it (it has already run the gate itself).
    cseq_gate: CseqGate,
    /// Rules a test is allowed to violate (a deliberate non-compliance fixture
    /// where Alice/Bob intentionally emit non-conforming SIP). The hard gate
    /// skips findings from these rule names. Shared (`Rc`) with [`CseqGate`] so a
    /// `allow_violation` registered before `finish`/Drop is honoured by both.
    allow_violations: Rc<RefCell<HashSet<String>>>,
    /// Per-`recv` wait bound handed to every [`Agent`] this harness binds. Small
    /// under a paused (simulated) clock — a parked `recv` auto-advances virtual
    /// time, so the bound only catches a genuinely stuck flow. A *real*-clock
    /// infra shape (an external SUT over real sockets) must widen it, since the
    /// wait is then real wall-clock latency. Sourced from the Endpoint config.
    recv_timeout: Duration,
    /// `(agent, anchor)` message labels a scenario attached via
    /// [`tag_anchor`](Harness::tag_anchor), surfaced on the [`RunReport`] for
    /// the E2E check engine (ADR-0019).
    anchors: Rc<RefCell<Vec<crate::anchors::AnchorTag>>>,
}

impl Harness {
    /// Start a session named `scenario_name`. The simulated fabric uses the
    /// default [`crate::SIMULATED_TRANSIT_DELAY_MS`] one-hop transit delay, so a
    /// sent datagram arrives that much later (mirrors a real network).
    /// Timestamps ride a test clock so a paused runtime gives deterministic
    /// report times (see `run::run`).
    pub fn new(scenario_name: impl Into<String>) -> Self {
        Self::with_transit_delay(scenario_name, crate::SIMULATED_TRANSIT_DELAY_MS)
    }

    /// Like [`new`](Harness::new) but with an explicit one-hop transit delay
    /// (ms). A delay of `0` is coerced to `1`: zero transit under a paused
    /// runtime is a determinism trap (the delivery `sleep(0)` races the txn →
    /// router → dispatcher pipeline, so timer cancels land after the timer
    /// fired). A non-zero delay makes each `recv` park auto-advance
    /// deterministically. See [`SimulatedSignalingNetwork::new`].
    pub fn with_transit_delay(scenario_name: impl Into<String>, transit_delay_ms: u64) -> Self {
        let transit_delay_ms = transit_delay_ms.max(1);
        let sim: Arc<dyn SignalingNetwork> =
            Arc::new(SimulatedSignalingNetwork::new(transit_delay_ms));
        Self::build(
            scenario_name.into(),
            sim,
            Clock::test_at(0),
            TransportKind::Fake,
            RECV_TIMEOUT,
        )
    }

    /// Start a session over a **caller-supplied** network + clock — the seam an
    /// E2E *Infra shape* uses to run the **same** scenario over either a
    /// `SimulatedSignalingNetwork` under a paused clock (fake) or a
    /// `RealSignalingNetwork` under a wall clock (real, external SUT). The only
    /// per-shape differences are transport + clock + the `recv_timeout`; the
    /// scenario body is identical (ADR-0018). `transport_kind` tags the recording
    /// (`Fake` / `Live` / `Hybrid`); `recv_timeout` comes from the Endpoint config
    /// (small for sim, wide for a real socket).
    pub fn with_network_and_clock(
        scenario_name: impl Into<String>,
        network: Arc<dyn SignalingNetwork>,
        clock: Clock,
        transport_kind: TransportKind,
        recv_timeout: Duration,
    ) -> Self {
        Self::build(
            scenario_name.into(),
            network,
            clock,
            transport_kind,
            recv_timeout,
        )
    }

    /// Shared constructor body: wrap the raw network in the recorder + the full
    /// default RFC 3261 / 3262 / 3264 wire-invariant suite (per-message peer rules
    /// + cross-message rules) so every harness run gets the same post-run "all
    /// clean" RFC check the live SIPp endpoints apply — a stale-CSeq probe, a
    /// mid-dialog tag/route mutation, an RFC 3262/3264 PRACK/offer-answer slip that
    /// a test UA would silently answer is caught at layer close.
    fn build(
        name: String,
        raw_network: Arc<dyn SignalingNetwork>,
        clock: Clock,
        transport_kind: TransportKind,
        recv_timeout: Duration,
    ) -> Self {
        let recorder = Recorder::with_clock(transport_kind, clock);
        let audit_opts = ScopedAuditOptions {
            rules: sip_net::rfc_peer_rules(),
            cross_message_rules: sip_net::rfc_cross_message_rules(),
            ..Default::default()
        };
        let wrapped = with_all_contracts(
            raw_network,
            recorder.clone(),
            RunContext::TestWithRecorder,
            audit_opts,
            true,
        );
        let dump = PanicDump {
            name: name.clone(),
            channel: wrapped.recording.channel(),
            recorder: recorder.clone(),
            armed: Cell::new(true),
        };
        let allow_violations: Rc<RefCell<HashSet<String>>> = Rc::new(RefCell::new(HashSet::new()));
        let cseq_gate = CseqGate {
            name: name.clone(),
            channel: wrapped.recording.channel(),
            armed: Cell::new(true),
            allow: allow_violations.clone(),
        };
        Self {
            network: wrapped.network,
            recording: wrapped.recording,
            recorder,
            ids: Arc::new(Ids(AtomicU64::new(1))),
            name,
            description: None,
            dump,
            cseq_gate,
            allow_violations,
            recv_timeout,
            anchors: Rc::new(RefCell::new(Vec::new())),
        }
    }

    /// Re-seed the shared branch/tag/Call-ID counter. The default (1) is
    /// deterministic so fake-fabric report bytes are stable across runs — but
    /// against a REAL, stateful, shared SUT (the kind cluster) every run would
    /// then mint the SAME Call-IDs and Via branches as the last one, and the
    /// SUT's transaction layer rightly treats the new INVITE as a
    /// retransmission of the finished call and replays its cached final
    /// response (RFC 3261 §17.2.3 absorption — observed as an instant 200 OK
    /// and a never-arriving b-leg). A real Infra shape seeds with wall-clock
    /// entropy so identifiers are unique across runs, as RFC 3261 §8.1.1.4
    /// demands of a real UA.
    pub fn seed_ids(&self, seed: u64) {
        self.ids.0.store(seed, Ordering::Relaxed);
    }

    /// Label a message `agent` received with a canonical **anchor** name
    /// (ADR-0019) — e.g. `tag_anchor(&bob1, "initialInvite", uas.request())`.
    /// The tag stores the message's identity keys; the E2E check engine
    /// resolves `<agent>.<anchor>` to the recorded wire entry post-call via
    /// [`RunReport::anchors`]. Tagging the same `(agent, anchor)` twice keeps
    /// both (resolution takes the first — re-tag deliberately, not by accident).
    pub fn tag_anchor(
        &self,
        agent: &Agent,
        anchor: impl Into<String>,
        keys: impl Into<crate::anchors::AnchorKeys>,
    ) {
        self.anchors.borrow_mut().push(crate::anchors::AnchorTag {
            agent: agent.name().to_string(),
            anchor: anchor.into(),
            agent_addr: agent.addr(),
            keys: keys.into(),
            sent: false,
        });
    }

    /// Set the report description (port of `.describe(...)`).
    pub fn describe(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Allow a single RFC rule to be violated on this run WITHOUT failing the
    /// hard gate — the **only** sanctioned way to deactivate a rule in a test.
    ///
    /// Reserve this for a deliberate non-compliance fixture, where Alice/Bob
    /// intentionally emit non-conforming SIP to exercise the B2BUA's reject /
    /// error path (e.g. an out-of-dialog BYE to drive a 481). The `justification`
    /// is mandatory and logged. It is **not** an escape hatch for a finding that
    /// reflects a real B2BUA bug — fix the B2BUA (or the test peer) instead.
    ///
    /// `rule` is the rule's `name()` (e.g. `"rfc3261.byeOnlyInDialog"`). The
    /// finding is still recorded (advisory) so the report shows what was waived.
    pub fn allow_violation(&self, rule: impl Into<String>, justification: impl Into<String>) {
        let rule = rule.into();
        let justification = justification.into();
        eprintln!(
            "[harness] RFC rule '{rule}' allowed to be violated on '{}': {justification}",
            self.name
        );
        self.allow_violations.borrow_mut().insert(rule);
    }

    /// Disarm the Drop-time RFC 3261 CSeq hard gate. For multi-SUT harnesses
    /// (e.g. the failover matrix) that wrap this `Harness` and run their OWN,
    /// **bind-scoped** CSeq audit at their layer-close: the generic gate here is
    /// unscoped — it audits every recorded bind, including the internal cluster
    /// workers, where a *transparent* failover legitimately splits one logical
    /// dialog's CSeq stream across nodes (CSeq 1 → primary, 2 → backup-takeover,
    /// 3 → reclaimed primary) and so reports a phantom "skip" no real UA sees.
    /// The wrapping harness substitutes the correct endpoint-scoped check, so it
    /// disarms this one to avoid a redundant, unscoped double-gate.
    pub fn disarm_cseq_gate(&self) {
        self.cseq_gate.disarm();
    }

    /// Declare and bind a named UA at `addr` (e.g. `"127.0.0.1:5060"`). The
    /// bind is role-tagged `{Uac, Uas}` — a test UA originates and answers but
    /// is never a proxy, so proxy-subject RFC rules (no-target-404,
    /// 100-within-200ms, strict-route rewrite, PRACK forwarding) do not judge
    /// its lane. Use [`agent_with_roles`](Self::agent_with_roles) to override.
    pub async fn agent(&self, name: impl Into<String>, addr: &str) -> Agent {
        self.agent_with_roles(name, addr, HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]))
            .await
    }

    /// [`agent`](Self::agent) with explicit bind roles for RFC-rule subject
    /// dispatch (e.g. a UA scripted to relay should declare `{Proxy}` too).
    pub async fn agent_with_roles(
        &self,
        name: impl Into<String>,
        addr: &str,
        roles: HashSet<sip_net::UaRole>,
    ) -> Agent {
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name.clone(), NetworkTag::Ext);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 64).with_roles(roles))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        let rr_fold = decide_rr_fold(&name);
        eprintln!(
            "[harness] UA {name}: Record-Route fold = {rr_fold:?} \
             (set HARNESS_RR_FOLD=separate|combined to pin)"
        );
        Agent {
            name: name.clone(),
            addr,
            uri: format!("sip:{name}@{}", addr.ip()),
            ep: Arc::from(ep),
            ids: self.ids.clone(),
            rr_fold,
            recv_timeout: self.recv_timeout,
            txn: Arc::new(TxnView::functional()),
            acks: Arc::new(AckObligations::default()),
        }
    }

    /// Begin a **multi-callee group** — several logical [`Agent`]s that share
    /// ONE bound socket at `addr`, demultiplexed by R-URI prefix (out-of-dialog)
    /// and Call-ID (in-dialog). This is how the transfer flows drive Bob /
    /// Charlie / David when the B2BUA egresses every callee leg to a single ROUTE
    /// target: `callee_group(addr).callee("bob", "049…").callee("charlie", "231…")
    /// .build().await`. See [`crate::callee_group`]. Both the direct and the
    /// via-proxy paths are supported (the demux is source-address-agnostic).
    pub fn callee_group(&self, addr: &str) -> crate::callee_group::CalleeGroupBuilder<'_> {
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        crate::callee_group::CalleeGroupBuilder::new(self, addr)
    }

    /// The shared recording-wrapped network — so the `callee_group` builder binds
    /// its one shared socket through the very fabric the agents use.
    pub(crate) fn network(&self) -> Arc<dyn SignalingNetwork> {
        self.network.clone()
    }

    /// Register a recorder lane (one per bound socket) — used by
    /// [`agent_with_roles`](Self::agent_with_roles) and the `callee_group`
    /// builder, which registers a single lane for the shared callee socket.
    pub(crate) fn register_lane(&self, addr: SocketAddr, name: String, tag: NetworkTag) {
        self.recorder.register_lane(addr, name, tag);
    }

    /// The shared monotonic id source, so `callee_group`'s logical agents mint
    /// branches/tags/Call-IDs from the same deterministic sequence as every other
    /// agent this harness binds.
    pub(crate) fn ids(&self) -> Arc<Ids> {
        self.ids.clone()
    }

    /// The per-`recv` wait bound handed to every agent (inherited by a
    /// `callee_group`'s logical agents).
    pub(crate) fn recv_timeout(&self) -> Duration {
        self.recv_timeout
    }

    /// Declare and bind a record-routing proxy / load balancer at `addr`.
    /// Role-tagged `{Proxy}` so the proxy-subject RFC rules judge this lane
    /// and the per-UA dialog rules do not.
    pub async fn proxy(&self, name: impl Into<String>, addr: &str) -> Proxy {
        Proxy {
            agent: self
                .agent_with_roles(name, addr, HashSet::from([sip_net::UaRole::Proxy]))
                .await,
        }
    }

    /// Bind a **System-Under-Test** endpoint on the shared, recording-wrapped
    /// fabric and register a `Core` lane for it. Returns the raw
    /// [`UdpEndpoint`] + its bound address so a real SUT (e.g. `sip-proxy`'s
    /// `ProxyCore`) can run its own recv loop against the same network the
    /// agents use — every `send_to`/`recv` still flows through the recorder, so
    /// the recording remains the trace. The caller owns the spawned loop (abort
    /// it on drop). This is the seam that lets the harness drive a real proxy,
    /// not just peer-to-peer agents (ADR-0006 → ADR-0009).
    pub async fn bind_sut(&self, name: impl Into<String>, addr: &str) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        self.bind_sut_with_roles(name, addr, sip_net::all_ua_roles()).await
    }

    /// [`bind_sut`](Self::bind_sut) with explicit bind roles for RFC-rule
    /// subject dispatch: a `ProxyCore` SUT declares `{Proxy}`, a B2BUA SUT
    /// `{Uac, Uas}` (it terminates each leg as a UA — proxy-subject rules like
    /// no-target-404 do not govern it). The roles ride the recorded
    /// `BindAcquire` summary, so the hard gate and the report projection
    /// dispatch rules per endpoint with no extra plumbing.
    pub async fn bind_sut_with_roles(
        &self,
        name: impl Into<String>,
        addr: &str,
        roles: HashSet<sip_net::UaRole>,
    ) -> (Box<dyn UdpEndpoint>, SocketAddr) {
        let name = name.into();
        let addr: SocketAddr = addr.parse().unwrap_or_else(|e| panic!("bad addr {addr:?}: {e}"));
        self.recorder.register_lane(addr, name, NetworkTag::Core);
        let ep = self
            .network
            .bind_udp(BindUdpOpts::new(addr, 256).with_roles(roles))
            .await
            .unwrap_or_else(|e| panic!("bind {addr} failed: {e}"));
        (ep, addr)
    }

    /// Advance virtual time by `d` (requires a paused runtime —
    /// `#[tokio::test(start_paused = true)]`). Advances in 100 ms chunks,
    /// mirroring the source's `TestClock.adjust` loop so in-flight delivery
    /// tasks observe intermediate values. Because the report's `at_ms` rides
    /// the same tokio clock (via `sip-clock`), the elapsed time shows up in the
    /// rendered timestamps. Call it *between* protocol events (after the message
    /// just sent has been `expect`ed) so each message keeps a clean send/receive
    /// timestamp.
    pub async fn advance(&self, d: Duration) {
        sip_clock::testkit::advance_in_100ms_chunks(d).await;
    }

    /// Drain the fabric before the trace snapshot: wait out in-flight
    /// datagrams (a txn-layer auto-ACK sent by the scenario's LAST receive is
    /// still in transit when `finish` runs), then yield the scheduler so a
    /// LIVE recv loop (the SUT's) reads what was delivered. Fabricates
    /// nothing: a passive test agent's queue is untouched — since
    /// newkahneed-036 ask A its unread datagrams are already recorded at
    /// DELIVERY (arrival is a wire fact, tagged `unconsumed` on the ladder),
    /// so the audit sees them without any explicit read.
    async fn settle_network(&self) {
        self.network.await_in_flight(Duration::from_millis(200)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        // The reads above may themselves send (an auto-answered keepalive) —
        // one more drain so the snapshot closes on a quiet wire.
        self.network.await_in_flight(Duration::from_millis(200)).await;
    }

    /// The recording decorator handle — lets a caller read the raw signaling
    /// event channel (`recording().channel().snapshot()`) to run an audit rule
    /// directly over the trace WITHOUT consuming the harness or invoking the
    /// structural layer-close checks. Used by long-lived multi-SUT harnesses
    /// (e.g. the failover matrix) that assert the RFC CSeq check mid-life.
    pub fn recording(&self) -> sip_net::RecordingSignalingNetwork {
        self.recording.clone()
    }

    /// Close the recording layer and return the [`RunReport`] (trace projected
    /// from the recording). Failures in the fluent flow panic in-line, so a
    /// returned report is by construction a passing run.
    ///
    /// MANDATORY HARD GATE: before returning, the recorded trace is checked against
    /// the RFC 3261 in-dialog CSeq rule(s); ANY finding `panic!`s, failing the
    /// test. This is the SIP-plane analogue of the universal "all clean" sweep,
    /// applied to EVERY harness run with no per-test opt-in. Only the cseq
    /// cross-message rules gate here — NOT the structural `close()` anomalies
    /// (inFlightImbalance / queueLeak), which legitimately occur in timeout / reap
    /// / stall fixtures and must not fail those tests.
    pub async fn finish(self) -> RunReport {
        // A returned report is by construction a passing run, so the post-mortem
        // dump is no longer wanted: disarm it before tearing the session down. The
        // Drop-time cseq backstop is likewise disarmed — `finish` runs the SAME
        // gate inline just below, so the Drop guard would only double-check.
        self.dump.disarm();
        self.cseq_gate.disarm();
        self.settle_network().await;
        let events = self.recording.channel().snapshot();
        // Hard gate on the RFC CSeq rule(s) BEFORE the structural close: a CSeq
        // violation must fail the test (a real UA would reject these). Skip if the
        // test is already unwinding so we never double-panic. The structural
        // close() anomalies are intentionally NOT gated.
        let cseq_findings = rfc_hard_gate_findings(&events, &self.allow_violations.borrow());
        if !cseq_findings.is_empty() && !std::thread::panicking() {
            panic!("{}", render_rfc_panic(&self.name, &cseq_findings));
        }
        let audit = self.recording.close().await;
        let anchors = self.anchors.borrow().clone();
        RunReport::from_recording(self.name, self.description, self.recorder, events, audit, anchors)
    }

    /// [`finish`](Self::finish) that **collects** the hard-gate findings
    /// instead of panicking — for executors (the e2e run core) that want a
    /// gating RFC violation to FAIL the cell *with the full report intact*
    /// (diagram, checks, findings table) rather than crash it report-less.
    /// The returned findings are exactly the set `finish` would have panicked
    /// on: non-advisory, subject-applicable to the originating bind's declared
    /// roles, and not waived via [`allow_violation`](Self::allow_violation).
    pub async fn finish_collecting(self) -> (RunReport, Vec<sip_net::RfcFinding>) {
        self.dump.disarm();
        self.cseq_gate.disarm();
        self.settle_network().await;
        let events = self.recording.channel().snapshot();
        let allow = self.allow_violations.borrow().clone();
        let gating: Vec<sip_net::RfcFinding> = sip_net::evaluate_rfc_findings(&events)
            .into_iter()
            .filter(|f| !f.advisory && !allow.contains(&f.rule))
            .collect();
        let audit = self.recording.close().await;
        let anchors = self.anchors.borrow().clone();
        let report = RunReport::from_recording(
            self.name,
            self.description,
            self.recorder,
            events,
            audit,
            anchors,
        );
        (report, gating)
    }
}

/// The RFC 3261 / 3262 / 3264 audit findings over a recorded trace that MUST
/// fail the test — the `(lane, detail)` pairs the hard gate panics on. Runs the
/// full default suite (per-message peer rules + cross-message rules), skipping:
///   - any `force_advisory()` rule (architectural divergences recorded but not
///     gated — they still reach the report via the layer-close `close()`);
///   - any finding whose rule `subject()` does not intersect the originating
///     bind's declared roles (default = all roles, so this only narrows when a
///     test sets roles);
///   - any rule a test explicitly waived via [`Harness::allow_violation`].
///
/// ONLY the audit rules run here (the structural layer-close anomalies —
/// in-flight imbalance, queue leaks — are deliberately not consulted), so
/// timeout / reap / stall fixtures are not gated. Shared by `Harness::finish`
/// and the `Harness` Drop guard so the SAME suite runs on every run with no
/// per-test opt-in. Empty ⇒ clean.
fn rfc_hard_gate_findings(
    events: &[layer_harness::Stamped<SignalingNetworkEvent>],
    allow: &HashSet<String>,
) -> Vec<(String, String)> {
    // One shared evaluator (sip-net) runs the suite with subject dispatch —
    // the SAME pass the report projection lists — so the gate and the report
    // can never disagree on which endpoint a rule applies to. The gate keeps
    // only the non-advisory, non-waived subset.
    sip_net::evaluate_rfc_findings(events)
        .into_iter()
        .filter(|f| !f.advisory && !allow.contains(&f.rule))
        .map(|f| (f.lane, f.detail))
        .collect()
}

/// Format the hard-gate panic message listing every RFC audit violation.
fn render_rfc_panic(name: &str, findings: &[(String, String)]) -> String {
    format!(
        "[{name}] SIP RFC audit violation(s) on the recorded trace — a real \
         UA would have rejected these, so this test MUST fail (the RFC check is a \
         mandatory hard gate; if a fixture deliberately violates a rule, waive it \
         with Harness::allow_violation):\n{}",
        findings
            .iter()
            .map(|(lane, detail)| format!("  • [{lane}] {detail}"))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

// ---------------------------------------------------------------------------
// Post-mortem trace dump
// ---------------------------------------------------------------------------

/// RAII trace dumper. A failing scenario `panic!`s — a `recv` timeout
/// (`Agent::recv`), an `expect` status mismatch (`expect_response`), a wrong
/// method (`Agent::receive`) — and aborts *before* [`Harness::finish`], the only
/// path that renders the recording. Without this guard the most common failure
/// (a message that never arrived / wrong method) yields a one-line panic and
/// zero visibility into what was actually on the wire.
///
/// This guard's `Drop` notices the in-flight unwind (`std::thread::panicking`)
/// and dumps a compact wire trace to stderr, so every panicking scenario
/// self-documents with no per-test instrumentation. `finish` disarms it (a
/// clean run already has its report). It projects the **synchronous**
/// `channel().snapshot()` — no async, no `close()` — and is best-effort and
/// panic-safe: it never panics inside `Drop` (a poisoned mutex etc. is
/// swallowed).
struct PanicDump {
    name: String,
    channel: Channel<SignalingNetworkEvent>,
    recorder: Recorder,
    armed: Cell<bool>,
}

impl PanicDump {
    fn disarm(&self) {
        self.armed.set(false);
    }

    /// Render the compact one-line-per-message trace from the recording.
    fn render(&self) -> String {
        let events = self.channel.snapshot();
        let entries = to_sip_entries(&events);
        let names: BTreeMap<SocketAddr, String> = self
            .recorder
            .snapshot()
            .lanes
            .into_iter()
            .map(|l| (l.addr, l.names.first().cloned().unwrap_or_default()))
            .collect();
        let label_for = |addr: &SocketAddr| match names.get(addr) {
            Some(n) if !n.is_empty() => format!("{n} ({addr})"),
            _ => addr.to_string(),
        };
        let base = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);

        let mut out = format!(
            "\n══ SIP trace for '{}' (dumped on panic — finish() not reached) ══\n",
            self.name
        );
        if entries.is_empty() {
            out.push_str("  (no messages recorded)\n");
        }
        for e in &entries {
            let sent = format_relative(e.sent_ms as i64 - base);
            let ts = match e.received_ms {
                Some(r) if r != e.sent_ms => format!("{sent} → {}", format_relative(r as i64 - base)),
                _ => sent,
            };
            let undelivered = if e.delivered { "" } else { "  [UNDELIVERED]" };
            out.push_str(&format!(
                "  [{ts}] {} → {}  {}{}\n",
                label_for(&e.from),
                label_for(&e.to),
                facets(&e.raw).label,
                undelivered
            ));
        }
        out.push_str(&format!("══ end SIP trace ({} message(s)) ══\n", entries.len()));
        out
    }
}

impl Drop for PanicDump {
    fn drop(&mut self) {
        if !self.armed.get() || !std::thread::panicking() {
            return;
        }
        // Never panic while already unwinding: a second panic in `Drop` aborts
        // the process. Swallow any failure (e.g. a poisoned mutex on snapshot).
        if let Ok(text) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.render())) {
            eprint!("{text}");
        }
    }
}

/// RAII backstop for the RFC 3261 CSeq hard gate when a [`Harness`] is dropped
/// WITHOUT [`Harness::finish`]. `finish` runs the gate inline and disarms this;
/// a harness left to drop (or whose scenario forgot to `finish`) still gets the
/// same mandatory check. On Drop, if still armed and the test is not already
/// unwinding, it computes the cross-message (cseq) findings over the recorded
/// channel and `panic!`s on any — failing the test. Only the cseq rules run here
/// (the structural layer-close anomalies are NOT consulted), so timeout / reap /
/// stall fixtures are not gated. The `!std::thread::panicking()` guard prevents
/// a double-panic when the test is already failing.
struct CseqGate {
    name: String,
    channel: Channel<SignalingNetworkEvent>,
    armed: Cell<bool>,
    /// Shared with the owning [`Harness`] so a deliberate-violation waiver
    /// registered via `allow_violation` is honoured by this Drop backstop too.
    allow: Rc<RefCell<HashSet<String>>>,
}

impl CseqGate {
    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for CseqGate {
    fn drop(&mut self) {
        if !self.armed.get() || std::thread::panicking() {
            return;
        }
        // Reading the snapshot + running the rules is panic-free in practice, but
        // guard it so a render fault can never turn into a double-panic abort.
        let allow = self.allow.borrow().clone();
        let findings = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            rfc_hard_gate_findings(&self.channel.snapshot(), &allow)
        })) {
            Ok(f) => f,
            Err(_) => return,
        };
        if !findings.is_empty() {
            panic!("{}", render_rfc_panic(&self.name, &findings));
        }
    }
}

// ---------------------------------------------------------------------------
// Transaction-layer receive view (newkahneed-034)
// ---------------------------------------------------------------------------

/// RFC 3261 §17.2 **once-and-only-once receive view** — the transaction-layer
/// dedup the raw `Agent` deliberately lacked (its doc used to say "surfaces
/// every duplicate datagram", and every functional test re-absorbed Timer A/E
/// retransmissions by hand via `receive_absorbing`/`receive_tolerating` lists
/// keyed on METHOD NAME, which over-approximates "a retransmission of the
/// request I already saw" into "any request of this method" — a genuinely new
/// request could be silently swallowed as noise).
///
/// Semantics (applied below the test-facing receive API, per [`Agent`] UA):
/// The one rule: **absorb only a provable duplicate.** A datagram is absorbed
/// iff it is BYTE-IDENTICAL to one already surfaced under the same key;
/// anything else surfaces. This can never mask a real signal (a genuine
/// message differs in bytes and is delivered) and can never wedge a liveness
/// flow (nothing panics).
/// - An inbound **request** is keyed (Call-ID, top-Via `branch`, method) —
///   §17.2.3 plus Call-ID so two different calls that reuse a branch (a
///   deterministic harness-proxy / crash-reboot id-reuse artifact) are not
///   conflated. A CANCEL/ACK sharing its INVITE's branch is its own key
///   (method differs). First arrival surfaces once; a byte-identical re-arrival
///   (Timer A/E) is absorbed; different bytes under the same key (deterministic
///   id reuse across a reboot) surface as new work.
/// - An inbound **final response** (>= 200) dedups the same way, keyed
///   (Call-ID, branch, CSeq, status). A byte-different same-key final — a
///   forked 2xx with a distinct To-tag — surfaces (a real signal).
/// - **Provisionals are NEVER deduped**: a byte-identical second 180 is a
///   legitimate B2BUA relay observable ("ring again", one a-leg early dialog —
///   see newkahneed-033 D2), not something the harness may hide.
/// - **No §17.2.1 response re-emission**: the simulated fabric is lossless, so
///   a duplicate only means the answer is still in transit (or the test has
///   deliberately not answered — the silent-callee case); re-emitting would
///   add trace noise without function. The load lane's `loadgen::mux::CallTxns`
///   owns re-answer semantics on real, lossy networks.
///
/// Recording and leak bookkeeping are unaffected: dedup happens after the
/// endpoint read, so the RFC-audit trace still contains every duplicate
/// datagram, and an absorbed read is marked received (no `queueLeak`).
///
/// The load-lane [`loadbind::AgentBinder`] constructs its agents in **wire
/// view** ([`TxnView::wire`]): the mux already dedups ahead of the agent
/// there, and double-dedup would silently change load semantics.
pub(crate) struct TxnView {
    /// Raw-surface opt-out ([`Agent::wire_view`]); shared by every clone of
    /// the UA so the whole logical endpoint drops to the wire together.
    wire: AtomicBool,
    /// First-seen raw bytes per server-transaction key
    /// (Call-ID, top-Via branch, request-line method). Call-ID is part of the
    /// key so two *different* calls that collide on a branch (a deterministic
    /// harness-proxy artifact — the real ProxyCore can mint the same forwarded
    /// branch across calls under the harness's seeded id source) are never
    /// mistaken for one transaction; a genuine §17.2.3 retransmission carries
    /// the same Call-ID by construction.
    requests: Mutex<HashMap<(String, String, String), Vec<u8>>>,
    /// First-seen raw bytes per final-response key
    /// (Call-ID, top-Via branch, CSeq number, CSeq method, status).
    finals: Mutex<HashMap<(String, String, u32, String, u16), Vec<u8>>>,
}

/// What the txn view decided about one inbound datagram.
#[derive(Debug, PartialEq, Eq)]
enum TxnVerdict {
    /// New work — hand it to the test.
    Surface,
    /// A byte-identical retransmission of something already surfaced — drop it
    /// below the API (the read is still recorded).
    Absorb,
}

impl TxnView {
    /// Functional-lane default: txn view ON.
    pub(crate) fn functional() -> Self {
        Self { wire: AtomicBool::new(false), requests: Mutex::default(), finals: Mutex::default() }
    }

    /// Load-lane default: raw wire surface (the mux owns dedup there).
    pub(crate) fn wire() -> Self {
        Self { wire: AtomicBool::new(true), requests: Mutex::default(), finals: Mutex::default() }
    }

    fn verdict(&self, raw: &[u8], msg: &SipMessage) -> TxnVerdict {
        if self.wire.load(Ordering::Relaxed) {
            return TxnVerdict::Surface;
        }
        match msg {
            SipMessage::Request(r) => {
                // Unkeyable (no top-Via branch / pre-RFC3261 cookie): surface —
                // graceful degradation to the old raw behaviour.
                let Some(branch) = top_via_branch(&r.headers) else {
                    return TxnVerdict::Surface;
                };
                if !branch.starts_with("z9hG4bK") {
                    return TxnVerdict::Surface;
                }
                let key = (r.call_id.clone(), branch, r.method.to_string());
                let mut seen = self.requests.lock().unwrap();
                match seen.get(&key) {
                    Some(first) if first.as_slice() == raw => TxnVerdict::Absorb,
                    _ => {
                        // New key, or same key with different bytes (deterministic
                        // id reuse across a reboot): deliver it, and remember the
                        // latest bytes so its OWN retransmits still dedup.
                        seen.insert(key, raw.to_vec());
                        TxnVerdict::Surface
                    }
                }
            }
            SipMessage::Response(r) if r.status >= 200 => {
                let Some(branch) = top_via_branch(&r.headers) else {
                    return TxnVerdict::Surface;
                };
                let key =
                    (r.call_id.clone(), branch, r.cseq.seq, r.cseq.method.to_string(), r.status);
                let mut seen = self.finals.lock().unwrap();
                match seen.get(&key) {
                    None => {
                        seen.insert(key, raw.to_vec());
                        TxnVerdict::Surface
                    }
                    Some(first) if first.as_slice() == raw => TxnVerdict::Absorb,
                    // Byte-different same-key final = a forked 2xx (distinct
                    // To-tag) — a real signal, surfaced.
                    Some(_) => TxnVerdict::Surface,
                }
            }
            // Provisionals: never deduped (ring-again is observable here).
            SipMessage::Response(_) => TxnVerdict::Surface,
        }
    }
}

/// §17.1.1.3 UAS-side ACK obligations (newkahneed-036 ask B) — the receive-side
/// mirror of 034-B's client auto-ACK: when a body answers an INVITE with a
/// **non-2xx final** through [`ServerTxn`]/[`Respond`], the transaction layer —
/// not the body — owns the arriving hop ACK.
///
/// Keyed `(Call-ID, INVITE top-Via branch)`, exactly like the SUT's own
/// synthesized hop ACK (the LB remembers the INVITE's forward branch and reuses
/// it) and the `rfc3261.unackedInviteNon2xxFinal` audit rule — so the
/// obligation, the wire, and the audit can never disagree on what matches.
///
/// **Order-independence is the point**: the ACK races the next transaction's
/// INVITE (the reroute shape: hop ACK for the 486 vs the rerouted INVITE) and
/// may land before or after it. Matching is by key, never positional:
/// - a matching ACK that would otherwise be a step error (a `receive("INVITE")`
///   that pulls the ACK first, a response wait that pulls a request) is
///   **absorbed** instead — the body never trips over it;
/// - a matching ACK that surfaces through a path that handles ACKs anyway (an
///   explicit `receive("ACK")`, `try_receive_tolerating_blocking`'s collect)
///   still **fulfils** the obligation — [`ServerTxn::expect_ack`] then returns
///   immediately;
/// - a matching ACK nothing ever pulls is still recorded at delivery
///   (036 ask A), so the gating wire rule discharges at `finish()` — that rule
///   IS the settle gate; no duplicate receive-side gate exists.
///
/// Shared across clones of one logical UA (like [`TxnView`]); works identically
/// in the load lane's wire view (claiming is independent of dedup).
#[derive(Default)]
pub(crate) struct AckObligations {
    /// `(Call-ID, INVITE top-Via branch)` → the hop ACK has been sighted.
    pending: Mutex<HashMap<(String, String), bool>>,
    /// Wakes an [`ServerTxn::expect_ack`] parked on fulfilment.
    notify: tokio::sync::Notify,
}

impl AckObligations {
    /// Open (or refresh — a retransmitted final re-arms the same key without
    /// clearing a sighting) the obligation for one rejected INVITE transaction.
    fn arm(&self, call_id: String, branch: String) {
        self.pending.lock().unwrap().entry((call_id, branch)).or_insert(false);
    }

    /// Record an ACK sighting. Returns `true` iff the key belongs to an armed
    /// obligation (fulfilled now or previously) — the caller may absorb it.
    fn note_ack(&self, call_id: &str, branch: &str) -> bool {
        let mut g = self.pending.lock().unwrap();
        match g.get_mut(&(call_id.to_string(), branch.to_string())) {
            Some(seen) => {
                *seen = true;
                drop(g);
                self.notify.notify_waiters();
                true
            }
            None => false,
        }
    }

    fn is_fulfilled(&self, call_id: &str, branch: &str) -> bool {
        self.pending
            .lock()
            .unwrap()
            .get(&(call_id.to_string(), branch.to_string()))
            .copied()
            .unwrap_or(false)
    }

    /// Park until the obligation is fulfilled — WITHOUT pulling from the inbox
    /// (the sighting itself is whoever pulls next — e.g. the actor reactor's
    /// `recv_any`, which claims a matching ACK below its API and so never
    /// surfaces it). This is the actor's wake for closing its `reject-final`
    /// ledger obligation. Never times out; callers bound it (a `select!` arm).
    async fn fulfilled(&self, call_id: &str, branch: &str) {
        loop {
            // Register interest BEFORE the check: `notify_waiters` only wakes
            // already-registered waiters, so check-then-wait would race a
            // sighting landing in between.
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.is_fulfilled(call_id, branch) {
                return;
            }
            notified.await;
        }
    }
}

/// The `branch` parameter of the topmost Via header, if any.
pub(crate) fn top_via_branch(headers: &[SipHeader]) -> Option<String> {
    let via = get_header(headers, "via")?;
    via.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("branch").then(|| v.trim().to_string())
    })
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// One inbound SIP message surfaced through the §17.2 receive view — a request
/// (as a UAS-side [`ServerTxn`]) or a response — WITHOUT asserting either the
/// kind or, for a request, the method. The reactive per-endpoint actor
/// ([`crate::actor`]) dispatches on this via [`Agent::recv_any`].
pub enum Inbound {
    /// A received request, wrapped in its UAS-side transaction.
    Request(ServerTxn),
    /// A received response (to one of our client transactions).
    Response(SipResponse),
}

/// A stateful fake UA. Cheap to clone (shares the endpoint + id source); the
/// dialog state lives on the per-transaction handles it returns, not here.
#[derive(Clone)]
pub struct Agent {
    // Fields are `pub(crate)` so the Send [`loadbind::AgentBinder`] can construct
    // an `Agent` the same way [`Harness::agent_with_roles`] does, without the
    // `!Send` `Harness` wrapper. The fluent API is the public surface.
    pub(crate) name: String,
    pub(crate) addr: SocketAddr,
    /// Dialog URI (`sip:name@ip`, no port) — used for From/To.
    pub(crate) uri: String,
    pub(crate) ep: Arc<dyn UdpEndpoint>,
    pub(crate) ids: Arc<Ids>,
    /// How this UA echoes multiple Record-Route rows when it acts as UAS
    /// ([`RecordRouteFold`]). Chosen per-UA at bind time.
    pub(crate) rr_fold: RecordRouteFold,
    /// Per-`recv` wait bound, inherited from the [`Harness`] (Endpoint config).
    pub(crate) recv_timeout: Duration,
    /// §17.2 once-and-only-once receive view ([`TxnView`], newkahneed-034).
    /// Shared across clones — one transaction table per logical UA.
    pub(crate) txn: Arc<TxnView>,
    /// §17.1.1.3 UAS-side ACK obligations ([`AckObligations`],
    /// newkahneed-036 ask B). Shared across clones — one table per logical UA.
    pub(crate) acks: Arc<AckObligations>,
}

impl Agent {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Drop this UA to the **raw wire surface**: disable the §17.2
    /// once-and-only-once receive view ([`TxnView`]) so EVERY duplicate
    /// datagram surfaces again. Reach for this ONLY when retransmission is
    /// the *subject* of the test (Timer A/E assertions, ring-again pinning,
    /// drop-rate recovery) — the same sanction rule as
    /// [`Harness::allow_violation`]. Affects every clone of this UA.
    pub fn wire_view(&self) {
        self.txn.wire.store(true, Ordering::Relaxed);
    }

    fn branch(&self) -> String {
        format!("z9hG4bK-{}-{}", self.name, self.ids.next())
    }
    fn tag(&self) -> String {
        format!("{}-tag-{}", self.name, self.ids.next())
    }
    fn via(&self) -> ViaSpec {
        ViaSpec {
            local_ip: self.addr.ip().to_string(),
            local_port: self.addr.port(),
            transport: SipTransport::Udp,
            branch: self.branch(),
            custom_params: vec![],
        }
    }
    /// A fresh top `Via` header value (new branch) — a new client transaction
    /// (RFC 3261 §8.1.1.7) for a resend (e.g. the §22.2 authenticated INVITE).
    fn via_header(&self) -> String {
        format!(
            "SIP/2.0/UDP {}:{};branch={}",
            self.addr.ip(),
            self.addr.port(),
            self.branch()
        )
    }
    fn contact(&self) -> ContactSpec {
        ContactSpec {
            user: self.name.clone(),
            host: self.addr.ip().to_string(),
            port: self.addr.port(),
            uri_params: vec![],
        }
    }

    /// Panicking veneer over [`try_send`](Agent::try_send).
    async fn send(&self, msg: &SipMessage, dst: SocketAddr) {
        unwrap_step(self.try_send(msg, dst).await)
    }

    /// Panicking veneer over [`try_recv`](Agent::try_recv).
    async fn recv(&self) -> SipMessage {
        unwrap_step(self.try_recv().await)
    }

    /// THE send core: one datagram out, a transport error returned as
    /// [`StepError::Transport`]. The functional lane panics on it via
    /// [`send`](Agent::send); the best-effort teardown helpers
    /// ([`Dialog::bye_best_effort`], [`CancelHandle`]) the load driver runs on
    /// a failed call swallow it — a send must never abort the worker.
    pub(crate) async fn try_send(
        &self,
        msg: &SipMessage,
        dst: SocketAddr,
    ) -> Result<(), StepError> {
        self.ep
            .send_to(&serialize(msg), dst)
            .await
            .map_err(|e| StepError::Transport { who: self.name.clone(), detail: e.to_string() })
    }

    /// THE receive core: one SIP datagram surfaced through the §17.2 receive
    /// view ([`TxnView`]). A timeout / closed queue / parse error is a
    /// [`StepError`]; the functional lane panics on it via [`recv`](Agent::recv).
    ///
    /// Every surfaced ACK is also SIGHTED against the §17.1.1.3 obligation
    /// table ([`AckObligations`], 036 ask B) — fulfilment is recorded here so
    /// it happens on ANY pull path, but the absorb decision stays with the
    /// caller ([`ack_obligation_claims`](Agent::ack_obligation_claims) at the
    /// would-be-error sites), so an explicit `receive("ACK")` keeps working.
    async fn try_recv(&self) -> Result<SipMessage, StepError> {
        loop {
            let pkt = match tokio::time::timeout(self.recv_timeout, self.ep.recv()).await {
                Err(_) => return Err(StepError::Timeout { who: self.name.clone() }),
                Ok(None) => return Err(StepError::QueueClosed { who: self.name.clone() }),
                Ok(Some(p)) => p,
            };
            let msg = CustomParser::new().parse(&pkt.raw).map_err(|e| StepError::Unparseable {
                who: self.name.clone(),
                detail: e.to_string(),
            })?;
            if let SipMessage::Request(r) = &msg {
                self.ack_obligation_claims(r);
            }
            match self.txn.verdict(&pkt.raw, &msg) {
                TxnVerdict::Surface => return Ok(msg),
                TxnVerdict::Absorb => continue,
            }
        }
    }

    /// Whether `r` is the hop ACK of an armed §17.1.1.3 obligation on this UA
    /// (036 ask B). Marks the obligation fulfilled (idempotent). A receive path
    /// that would otherwise ERROR on an unexpected ACK calls this and absorbs
    /// instead — the ACK-races-the-next-INVITE interleave, in either order,
    /// never trips a body.
    pub(crate) fn ack_obligation_claims(&self, r: &SipRequest) -> bool {
        r.method.as_str() == "ACK"
            && top_via_branch(&r.headers).is_some_and(|b| self.acks.note_ack(&r.call_id, &b))
    }

    /// THE request-receive core: receive the next request and check its method,
    /// returning a UAS-side transaction. A wrong method, an unexpected
    /// response, a timeout — all become a [`StepError`]; the functional lane
    /// panics on them via [`receive`](Agent::receive).
    ///
    /// A txn-owned hop ACK (an armed §17.1.1.3 obligation, 036 ask B) that
    /// arrives ahead of the awaited request is absorbed, not an error — the
    /// ACK-before-the-next-INVITE interleave needs no body-side boilerplate.
    pub async fn try_receive(&self, method: &str) -> Result<ServerTxn, StepError> {
        loop {
            match self.try_recv().await? {
                SipMessage::Request(r) => {
                    if r.method != method {
                        if self.ack_obligation_claims(&r) {
                            continue;
                        }
                        return Err(StepError::WrongMethod {
                            who: self.name.clone(),
                            expected: method.to_string(),
                            got: r.method.to_string(),
                        });
                    }
                    return Ok(ServerTxn::from_request(self.clone(), r));
                }
                SipMessage::Response(r) => {
                    return Err(StepError::UnexpectedKind {
                        who: self.name.clone(),
                        detail: format!(
                            "got a {} {} response, expected a {method} request",
                            r.status, r.reason
                        ),
                    })
                }
            }
        }
    }

    /// Receive the next inbound message of EITHER kind through the shared §17.2
    /// receive view ([`TxnView`]) — the reactive-actor primitive (the
    /// [`crate::actor`] reactor dispatches on this instead of asserting one
    /// expected message, so a late / reordered / retransmitted datagram is
    /// always consumed). A timeout / closed queue / parse error is a
    /// [`StepError`] exactly as [`try_receive`](Agent::try_receive) returns
    /// (the reactor treats `Timeout` as "loop again", `QueueClosed` as fatal).
    ///
    /// Unlike [`try_receive`](Agent::try_receive) it neither asserts a method
    /// nor auto-answers anything — the reactor's `default_react` owns the answer
    /// policy. A txn-owned §17.1.1.3 hop ACK (036 ask B, an armed obligation) is
    /// still absorbed below the API: it is the transaction layer's to claim,
    /// never the reactor's to see. A NORMAL ACK (to our own 2xx, no armed
    /// obligation) surfaces as `Inbound::Request` so the reactor records it.
    pub async fn recv_any(&self) -> Result<Inbound, StepError> {
        loop {
            match self.try_recv().await? {
                SipMessage::Request(r) => {
                    // An armed non-2xx hop ACK is the txn layer's; a plain 2xx
                    // ACK is not armed and surfaces (idempotent re-sight — the
                    // receive core already sighted it, `note_ack` is a no-op).
                    if self.ack_obligation_claims(&r) {
                        continue;
                    }
                    return Ok(Inbound::Request(ServerTxn::from_request(self.clone(), r)));
                }
                SipMessage::Response(r) => return Ok(Inbound::Response(r)),
            }
        }
    }

    /// Park until the §17.1.1.3 hop ACK for the given INVITE server transaction
    /// (`(Call-ID, top-Via branch)`) has been SIGHTED by the receive core —
    /// the non-pulling twin of [`ServerTxn::expect_ack`], for the reactive
    /// actor: its own `recv_any` claims the ACK below the API (never surfacing
    /// it), and this future is how the actor still observes the fulfilment
    /// (closing its `reject-final` ledger obligation). Never times out; run it
    /// as a bounded `select!` arm.
    pub(crate) async fn hop_ack_fulfilled(&self, call_id: &str, branch: &str) {
        self.acks.fulfilled(call_id, branch).await
    }

    /// Best-effort drain-and-200 for the load driver's teardown: for up to
    /// `window`, receive any inbound request and answer it `200 OK` (Via-routed),
    /// then return when the window elapses or the socket goes quiet. After a
    /// failed call's a-leg has been BYE'd, this lets the in-process callee answer
    /// the SUT's relayed b-leg BYE so the SUT closes its b-leg promptly instead of
    /// waiting out a retransmit Timer. Never panics (sends are best-effort).
    pub async fn quiesce(&self, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            match tokio::time::timeout(remaining, self.ep.recv()).await {
                Ok(Some(pkt)) => {
                    if let Ok(SipMessage::Request(r)) = CustomParser::new().parse(&pkt.raw) {
                        let resp =
                            generate_response(&r, 200, "OK", &GenerateResponseOpts::default());
                        let dst = top_via_addr(&r).unwrap_or(self.addr);
                        let _ = self.try_send(&SipMessage::Response(resp), dst).await;
                    }
                }
                _ => return, // timed out or queue closed
            }
        }
    }

    /// Begin an out-of-dialog INVITE to `peer`. Returns a builder; call
    /// [`Invite::send`] (optionally after [`Invite::with_sdp`] / [`Invite::through`]).
    pub fn invite<'a>(&'a self, peer: &'a Agent) -> Invite<'a> {
        Invite {
            caller: self,
            peer,
            sdp: None,
            extra_headers: vec![],
            wire_dst: None,
            from_uri: None,
            to_uri: None,
            request_uri: None,
        }
    }

    /// Begin a generic **out-of-dialog** request of any [`OutOfDialogMethod`]
    /// (OPTIONS, MESSAGE, SUBSCRIBE, …) addressed to `peer` — the any-method
    /// sibling of [`invite`](Agent::invite). The mechanical SIP layer (Via +
    /// fresh branch, From-tag, Call-ID, CSeq, Contact, Max-Forwards,
    /// Content-Type/Length) is auto-filled exactly like the INVITE path; the
    /// caller supplies only headers/body. Returns a builder; finish with the
    /// fallible [`OutOfDialogRequest::try_send`] (load lane) or the panicking
    /// [`OutOfDialogRequest::send`] (functional tests).
    ///
    /// For a dialog-CREATING INVITE keep using [`invite`](Agent::invite) — this
    /// builder tracks no dialog state (a non-INVITE out-of-dialog transaction
    /// creates none).
    pub fn request<'a>(&'a self, method: OutOfDialogMethod, peer: &'a Agent) -> OutOfDialogRequest<'a> {
        OutOfDialogRequest {
            caller: self,
            peer,
            method,
            body: None,
            content_type: None,
            extra_headers: vec![],
            wire_dst: None,
            from_uri: None,
            to_uri: None,
            request_uri: None,
        }
    }

    /// Receive the next request and assert its method. Returns a UAS-side
    /// transaction handle for sending responses. Panicking veneer over
    /// [`try_receive`](Agent::try_receive).
    pub async fn receive(&self, method: &str) -> ServerTxn {
        unwrap_step(self.try_receive(method).await)
    }

    /// Non-blocking variant of [`receive_tolerating`](Agent::receive_tolerating):
    /// drain (and 200-OK) any *currently queued* `tolerate` requests, and return
    /// `Some(txn)` for the first queued `method` request — or `None` if the queue
    /// is empty (no datagram pending) *without* waiting. Lets a caller poll-advance
    /// the paused clock toward an unknown timer deadline in sub-reap steps: advance
    /// a little, drain, and stop the instant the awaited request appears (CLAUDE.md:
    /// drive between advances; never blow past the deadline + its reap in one step).
    /// Panics only on a *queued* request that is neither `method` nor tolerated.
    pub async fn try_receive_tolerating(
        &self,
        method: &str,
        tolerate: &[&str],
    ) -> Option<ServerTxn> {
        while let Some(pkt) = self.ep.try_recv() {
            let msg = CustomParser::new()
                .parse(&pkt.raw)
                .unwrap_or_else(|e| panic!("{} received an unparseable datagram: {e}", self.name));
            match self.txn.verdict(&pkt.raw, &msg) {
                TxnVerdict::Surface => {}
                TxnVerdict::Absorb => continue,
            }
            let r = match msg {
                SipMessage::Request(r) => r,
                SipMessage::Response(r) => panic!(
                    "{} expected a {method} request, got a {} {} response",
                    self.name, r.status, r.reason
                ),
            };
            let mut txn = ServerTxn::from_request(self.clone(), r);
            if txn.request.method == method {
                return Some(txn);
            }
            if self.ack_obligation_claims(&txn.request) {
                continue; // txn-owned §17.1.1.3 hop ACK (036 ask B)
            }
            if tolerate.iter().any(|t| txn.request.method == *t) {
                txn.respond(200, "OK").send().await;
                continue;
            }
            panic!(
                "{} expected a {method} request (tolerating {tolerate:?}), got {}",
                self.name, txn.request.method
            );
        }
        None
    }

    /// **Blocking**, fallible, tolerant receive — the load-lane primitive that
    /// replaces `quiesce`-as-choreography (newkahneed-033 ask C): wait (up to
    /// `recv_timeout` per datagram) until a `method` request arrives, answering
    /// every interleaved `tolerate` request `200 OK` along the way, and return
    /// the matched [`ServerTxn`] **plus everything that was absorbed** so the
    /// body can ASSERT the interleaved traffic instead of blind-draining it.
    ///
    /// Contract details that make this assertable where `quiesce` is not:
    /// - a missing `method` request is a [`StepError::Timeout`] — a lost BYE
    ///   can no longer masquerade as success;
    /// - an interleaved **ACK** is absorbed + collected WITHOUT a response (an
    ///   ACK completes a transaction and must never be answered), whether or
    ///   not it is listed in `tolerate`;
    /// - a tolerated **offer-carrying INVITE/UPDATE** (a realign re-INVITE) is
    ///   answered `200` **with an SDP answer** — RFC 3264 §5 / RFC 3261
    ///   §13.3.1.1 forbid the bodyless 200 that `quiesce`'s bare drain sends;
    /// - any other method (or an inbound response) is an error, not a silent
    ///   200 — the strict-agent contract survives.
    pub async fn try_receive_tolerating_blocking(
        &self,
        method: &str,
        tolerate: &[&str],
    ) -> Result<(ServerTxn, Vec<SipRequest>), StepError> {
        let mut absorbed = Vec::new();
        loop {
            let r = match self.try_recv().await? {
                SipMessage::Request(r) => r,
                SipMessage::Response(r) => {
                    return Err(StepError::UnexpectedKind {
                        who: self.name.clone(),
                        detail: format!(
                            "got a {} {} response, expected a {method} request (tolerating {tolerate:?})",
                            r.status, r.reason
                        ),
                    })
                }
            };
            let mut txn = ServerTxn::from_request(self.clone(), r);
            if txn.request.method == method {
                return Ok((txn, absorbed));
            }
            if txn.request.method.as_str() == "ACK" {
                absorbed.push(txn.request);
                continue;
            }
            if tolerate.iter().any(|t| txn.request.method == *t) {
                let is_offer_reinvite = matches!(txn.request.method.as_str(), "INVITE" | "UPDATE")
                    && !txn.request.body.is_empty();
                let respond = txn.respond(200, "OK");
                if is_offer_reinvite {
                    respond.with_sdp(crate::callflow::ANSWER_SDP).try_send().await?;
                } else {
                    respond.try_send().await?;
                }
                absorbed.push(txn.request);
                continue;
            }
            return Err(StepError::WrongMethod {
                who: self.name.clone(),
                expected: format!("{method} (tolerating {tolerate:?})"),
                got: txn.request.method.to_string(),
            });
        }
    }

    /// **Best-effort socket drain** — read (and discard) every datagram *currently
    /// queued* at this UA without waiting, asserting nothing about them. Each read
    /// goes through the recording layer, so a message the scenario delivered but
    /// never explicitly `receive`d (a relayed final response the test didn't await,
    /// a retransmit toward a deliberately-silent peer) is recorded as **received**
    /// rather than surfacing as "lost in transit" / a `queueLeak` at bind close.
    ///
    /// This models a real always-on UA: its kernel keeps reading the socket even
    /// after the application is done driving the call. Pair it with a clock pump
    /// (e.g. [`FailoverHarness::linger_peers`]) so in-flight datagrams first land in
    /// the queue, then drain. Returns the number of datagrams drained.
    pub async fn drain(&self) -> usize {
        let mut n = 0;
        while self.ep.try_recv().is_some() {
            n += 1;
        }
        n
    }

    /// Like [`receive`](Agent::receive), but first drains (and 200-OKs) any
    /// requests whose method is in `tolerate` — the harness equivalent of the
    /// TS `allowExtra(method)`. Under a paused clock an advance that crosses a
    /// timer deadline emits a request whose 2xx round-trip races the txn-layer
    /// retransmit, so several identical copies queue before the awaited message
    /// (CLAUDE.md: tolerate retransmits, don't relax the assertion). Returns the
    /// first request matching `method`.
    pub async fn receive_tolerating(&self, method: &str, tolerate: &[&str]) -> ServerTxn {
        loop {
            let msg = self.recv().await;
            match msg {
                SipMessage::Request(r) => {
                    if r.method == method {
                        return ServerTxn::from_request(self.clone(), r);
                    }
                    if self.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK (036 ask B)
                    }
                    if tolerate.iter().any(|t| r.method == *t) {
                        // Drain + answer the duplicate so the txn layer stops
                        // retransmitting it, then keep waiting for `method`.
                        let mut txn = ServerTxn::from_request(self.clone(), r);
                        txn.respond(200, "OK").send().await;
                        continue;
                    }
                    panic!(
                        "{} expected a {method} request (tolerating {tolerate:?}), got {}",
                        self.name, r.method
                    );
                }
                SipMessage::Response(r) => panic!(
                    "{} expected a {method} request, got a {} {} response",
                    self.name, r.status, r.reason
                ),
            }
        }
    }

    /// Like [`receive`](Agent::receive), but SILENTLY absorbs (drops, sends NO
    /// response) any queued requests whose method is in `absorb` before returning
    /// the first request matching `method`.
    ///
    /// NOTE (newkahneed-034): the agent now has a §17.2 receive view
    /// ([`TxnView`]) that absorbs **byte-identical retransmissions**
    /// automatically, so most historical uses of this helper are no longer
    /// needed. It remains for (a) [`wire_view`](Agent::wire_view) agents, and
    /// (b) absorbing *distinct* same-method requests — which it matches by
    /// METHOD NAME ONLY, so a genuinely unexpected request of that method is
    /// masked too. Prefer a plain [`receive`](Agent::receive) first.
    ///
    /// The load-bearing difference from [`receive_tolerating`](Agent::receive_tolerating)
    /// is that the absorbed request is **not** `200 OK`'d: a 200 would ANSWER it.
    /// The intended case is a b-leg INVITE the callee is deliberately leaving
    /// unanswered (a **silent callee**, no `>=180`) while the UAC's Timer A keeps
    /// retransmitting the INVITE hop-by-hop through a front proxy that absorbs the
    /// callee's bare `100 Trying` (RFC 3261 §16.7) — so the UAC never quiesces its
    /// retransmit timer and the duplicates queue ahead of the message under test
    /// (the internally-originated CANCEL, the crossing-200's ACK, the reap BYE).
    pub async fn receive_absorbing(&self, method: &str, absorb: &[&str]) -> ServerTxn {
        loop {
            match self.recv().await {
                SipMessage::Request(r) => {
                    if r.method == method {
                        return ServerTxn::from_request(self.clone(), r);
                    }
                    if absorb.iter().any(|t| r.method == *t) {
                        // Drop the retransmission silently — no response (a UAS that
                        // has only 100'd its INVITE absorbs retransmits, replaying at
                        // most the 100 the proxy already eats). Keep waiting.
                        continue;
                    }
                    if self.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK (036 ask B)
                    }
                    panic!(
                        "{} expected a {method} request (absorbing {absorb:?}), got {}",
                        self.name, r.method
                    );
                }
                SipMessage::Response(r) => panic!(
                    "{} expected a {method} request, got a {} {} response",
                    self.name, r.status, r.reason
                ),
            }
        }
    }

    /// Send an out-of-dialog REFER addressed to `dst` whose To carries a bogus
    /// tag and whose Request-URI carries a `callRef` the B2BUA never minted — so
    /// the router resolves the (non-existent) call, finds no state, and rejects
    /// it 481 (`maybe_reject_orphan`). Used by the out-of-dialog REFER reject
    /// scenario. Returns a client-transaction handle to `expect` the 481 on.
    pub async fn send_out_of_dialog_refer(
        &self,
        dst: SocketAddr,
        refer_to: &str,
    ) -> InDialogTxn {
        // A synthetic dialog the B2BUA has never seen: fresh Call-ID, a bogus
        // remote (To) tag, and a remote target carrying a bogus stamped callRef
        // (unreserved chars → no escaping needed; the router reads it verbatim),
        // so resolution succeeds but hydration fails → the orphan 481 path.
        let view = StackDialog {
            call_id: format!("orphan-{}-{}", self.name, self.ids.next()),
            local_tag: self.tag(),
            remote_tag: "bogus-refer-tag".into(),
            local_uri: self.uri.clone(),
            remote_uri: format!("sip:unknown@{}", dst.ip()),
            remote_target: format!(
                "sip:unknown@{}:{};callRef=w0-orphan-bogus;leg=b-1",
                dst.ip(),
                dst.port()
            ),
            local_cseq: 0,
            route_set: vec![],
        };
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.via()),
            contact: Some(self.contact()),
            extra_headers: vec![SipHeader {
                name: "Refer-To".into(),
                value: refer_to.into(),
            }],
            ..Default::default()
        };
        let res = generate_in_dialog_request(InDialogMethod::Refer, &view, &opts);
        self.send(&SipMessage::Request(res.request), dst).await;
        InDialogTxn {
            agent: self.clone(),
            // A REFER's finals take no ACK.
            invite: None,
            wire_dst: dst,
        }
    }

    /// REGISTER this UA's AOR → its own Contact with a `registrar` front proxy,
    /// then wait for the 200 OK. A faithful mimic of a SIP UA's register step
    /// (RFC 3261 §10.2): the AOR is `aor` (the To/From URI), the Contact is this
    /// agent's `sip:name@ip:port`, and `ttl_sec` becomes the `Expires` the
    /// registrar grants. Returns the granted `Expires` (seconds) parsed back off
    /// the 200's `Expires` header, so a caller can assert / schedule a refresh
    /// (re-REGISTER) before it lapses. Out-of-dialog, no dialog is created.
    ///
    /// `aor` is the address-of-record URI (e.g. `sip:bob@example.com`); its
    /// userpart is what the registrar keys the binding on (matching sipjs's
    /// `sip-front-proxy/Registrar`). Send `ttl_sec = 0` to de-register.
    pub async fn register(&self, registrar: SocketAddr, aor: &str, ttl_sec: u32) -> u32 {
        let call_id = format!("reg-{}-{}@{}", self.name, self.ids.next(), self.addr.ip());
        let opts = GenerateOutOfDialogRequestOpts {
            // The REGISTER Request-URI is the registrar (domain), not a user.
            request_uri: format!("sip:{}", registrar.ip()),
            call_id,
            from_uri: aor.to_string(),
            from_tag: self.tag(),
            to_uri: aor.to_string(),
            to_tag: None,
            cseq: 1,
            via: Some(self.via()),
            // The Contact the registrar stores verbatim is this agent's wire
            // address (`sip:name@ip:port`) — the standard generated Contact.
            contact: Some(self.contact()),
            max_forwards: Some(70),
            body: vec![],
            content_type: None,
            // The requested binding lifetime (RFC 3261 §10.2.1.1).
            extra_headers: vec![SipHeader {
                name: "Expires".into(),
                value: ttl_sec.to_string(),
            }],
        };
        let req = generate_out_of_dialog_request(OutOfDialogMethod::Register, &opts);
        self.send(&SipMessage::Request(req), registrar).await;
        let resp = expect_response(self, 200, None).await;
        // Echo back the Expires the registrar actually granted (RFC 3261 §10.3
        // step 8): the registrar may clamp our request; the UA refreshes on it.
        get_header(&resp.headers, "expires")
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(ttl_sec)
    }
}

// ---------------------------------------------------------------------------
// Outgoing INVITE builder + client transaction
// ---------------------------------------------------------------------------

/// Builder for an outgoing INVITE (lets the SDP offer be attached fluently).
pub struct Invite<'a> {
    caller: &'a Agent,
    peer: &'a Agent,
    sdp: Option<String>,
    extra_headers: Vec<SipHeader>,
    /// Wire destination override — the INVITE is *addressed* to `peer` (its
    /// Contact is the Request-URI) but *sent* here. Set by [`Invite::through`]
    /// to route an initial INVITE via a proxy/LB.
    wire_dst: Option<SocketAddr>,
    /// Optional From/To/Request-URI overrides — the seam an E2E *Test case*
    /// uses to drive From/To/R-URI from input data (numbers) instead of the
    /// default `sip:name@ip` agent identities. `None` keeps the default.
    from_uri: Option<String>,
    to_uri: Option<String>,
    request_uri: Option<String>,
}

impl<'a> Invite<'a> {
    /// Attach an SDP offer body.
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Attach an arbitrary extra header on the initial INVITE (e.g. `Supported:
    /// 100rel, timer` to drive the 18x-management strategies).
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Send the initial INVITE to `proxy` instead of directly to the peer (the
    /// Request-URI still targets the peer). Used to drive an LB/record-routing
    /// proxy; subsequent in-dialog requests then follow the route set learned
    /// from the proxy's Record-Route automatically.
    /// Override the From URI (e.g. `"sip:+33123456789@example.com"`) — drives
    /// From from Test-case input instead of the default `sip:caller@ip`.
    pub fn from(mut self, uri: impl Into<String>) -> Self {
        self.from_uri = Some(uri.into());
        self
    }

    /// Override the To URI — drives To from Test-case input. The To URI also
    /// seeds the dialog's remote URI.
    pub fn to(mut self, uri: impl Into<String>) -> Self {
        self.to_uri = Some(uri.into());
        self
    }

    /// Override the Request-URI — drives the R-URI from Test-case input. The
    /// INVITE is still *sent* to the peer/`through` wire destination.
    pub fn ruri(mut self, uri: impl Into<String>) -> Self {
        self.request_uri = Some(uri.into());
        self
    }

    pub fn through(mut self, proxy: SocketAddr) -> Self {
        self.wire_dst = Some(proxy);
        self
    }

    /// Generate the INVITE (all headers filled in), send it, and return the
    /// client transaction handle.
    pub async fn send(self) -> ClientInvite {
        let caller = self.caller;
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let call_id = format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip());
        let from_tag = caller.tag();
        // Default identities are the agent URIs / a peer-addressed R-URI; a Test
        // case may override any of From/To/R-URI from its input data.
        let request_uri = self
            .request_uri
            .clone()
            .unwrap_or_else(|| format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port()));
        let from_uri = self.from_uri.clone().unwrap_or_else(|| caller.uri.clone());
        let to_uri = self.to_uri.clone().unwrap_or_else(|| peer.uri.clone());

        let opts = GenerateOutOfDialogRequestOpts {
            request_uri: request_uri.clone(),
            call_id: call_id.clone(),
            from_uri: from_uri.clone(),
            from_tag: from_tag.clone(),
            to_uri: to_uri.clone(),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            body: self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            content_type: None,
            extra_headers: self.extra_headers.clone(),
        };
        let invite = generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts);
        caller.send(&SipMessage::Request(invite.clone()), wire_dst).await;

        let dialog = StackDialog {
            call_id,
            local_tag: from_tag,
            remote_tag: String::new(),
            local_uri: from_uri,
            remote_uri: to_uri,
            remote_target: request_uri,
            local_cseq: 1,
            route_set: vec![],
        };
        ClientInvite {
            agent: caller.clone(),
            fallback_addr: peer.addr,
            wire_dst,
            original_invite: invite,
            dialog,
            fork_cseq: HashMap::new(),
        }
    }
}

/// Builder for a generic out-of-dialog request (any [`OutOfDialogMethod`]) —
/// see [`Agent::request`]. Mirrors [`Invite`]'s knobs (extra headers, optional
/// body, `through` wire routing, From/To/R-URI overrides) with a **fallible**
/// send for the load lane.
pub struct OutOfDialogRequest<'a> {
    caller: &'a Agent,
    peer: &'a Agent,
    method: OutOfDialogMethod,
    body: Option<Vec<u8>>,
    /// Content-Type for a non-empty body (defaults to `application/sdp`).
    content_type: Option<String>,
    extra_headers: Vec<SipHeader>,
    /// Wire destination override (send via a proxy/SUT; R-URI still targets peer).
    wire_dst: Option<SocketAddr>,
    from_uri: Option<String>,
    to_uri: Option<String>,
    request_uri: Option<String>,
}

impl<'a> OutOfDialogRequest<'a> {
    /// Attach an SDP body (`Content-Type: application/sdp`).
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.body = Some(sdp.as_bytes().to_vec());
        self.content_type = None;
        self
    }

    /// Attach an arbitrary body with an explicit Content-Type.
    pub fn with_body(mut self, content_type: &str, body: impl Into<Vec<u8>>) -> Self {
        self.body = Some(body.into());
        self.content_type = Some(content_type.to_string());
        self
    }

    /// Attach an arbitrary extra header. Repeatable; order preserved.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader { name: name.to_string(), value: value.to_string() });
        self
    }

    /// Send the request to `proxy` instead of directly to the peer (the
    /// Request-URI still targets the peer) — the same wire routing as
    /// [`Invite::through`].
    pub fn through(mut self, proxy: SocketAddr) -> Self {
        self.wire_dst = Some(proxy);
        self
    }

    /// Override the From URI.
    pub fn from(mut self, uri: impl Into<String>) -> Self {
        self.from_uri = Some(uri.into());
        self
    }

    /// Override the To URI.
    pub fn to(mut self, uri: impl Into<String>) -> Self {
        self.to_uri = Some(uri.into());
        self
    }

    /// Override the Request-URI.
    pub fn ruri(mut self, uri: impl Into<String>) -> Self {
        self.request_uri = Some(uri.into());
        self
    }

    /// Generate the request (all mechanical headers filled in), send it
    /// **fallibly**, and return the client transaction to
    /// [`try_expect`](InDialogTxn::try_expect) the response on. A transport
    /// failure surfaces as [`StepError::Transport`], never a panic.
    pub async fn try_send(self) -> Result<InDialogTxn, StepError> {
        let caller = self.caller;
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let request_uri = self
            .request_uri
            .unwrap_or_else(|| format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port()));
        let opts = GenerateOutOfDialogRequestOpts {
            request_uri,
            call_id: format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip()),
            from_uri: self.from_uri.unwrap_or_else(|| caller.uri.clone()),
            from_tag: caller.tag(),
            to_uri: self.to_uri.unwrap_or_else(|| peer.uri.clone()),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            body: self.body.unwrap_or_default(),
            content_type: self.content_type,
            extra_headers: self.extra_headers,
        };
        let req = generate_out_of_dialog_request(self.method, &opts);
        let msg = SipMessage::Request(req);
        caller.try_send(&msg, wire_dst).await?;
        let SipMessage::Request(request) = msg else { unreachable!() };
        Ok(InDialogTxn {
            agent: caller.clone(),
            // An out-of-dialog INVITE's non-2xx final takes a txn-layer ACK
            // (§17.1.1.3) — retain the request so the txn can build it.
            invite: matches!(self.method, OutOfDialogMethod::Invite).then_some(request),
            wire_dst,
        })
    }

    /// Panicking [`try_send`](Self::try_send) for functional tests.
    pub async fn send(self) -> InDialogTxn {
        unwrap_step(self.try_send().await)
    }

    /// **RFC 3261 §22.2 authenticated send** — the out-of-dialog twin of the
    /// INVITE choreography's auth retry (see [`crate::realcall::auth`]), for a
    /// future REGISTER / OPTIONS shape against a challenging registrar. Sends the
    /// request, awaits its `expect` final; if it is a `401`/`407` and `responder`
    /// is `Some`, adds the credential (a non-INVITE final needs no ACK,
    /// RFC 3261 §17.1.2.2), bumps the CSeq, and resends ONCE with a fresh Via
    /// branch, then awaits again. `responder == None` (the default) makes this a
    /// plain send-and-await with no retry — a `401`/`407` surfaces as
    /// `WrongStatus`, exactly as `try_send` + `try_expect` would.
    pub async fn try_send_authed(
        self,
        responder: Option<&dyn ChallengeResponder>,
        expect: u16,
    ) -> Result<SipResponse, StepError> {
        let caller = self.caller.clone();
        let peer = self.peer;
        let wire_dst = self.wire_dst.unwrap_or(peer.addr);
        let request_uri = self
            .request_uri
            .clone()
            .unwrap_or_else(|| format!("sip:{}@{}:{}", peer.name, peer.addr.ip(), peer.addr.port()));
        let mut opts = GenerateOutOfDialogRequestOpts {
            request_uri: request_uri.clone(),
            call_id: format!("{}-{}@{}", caller.name, caller.ids.next(), caller.addr.ip()),
            from_uri: self.from_uri.clone().unwrap_or_else(|| caller.uri.clone()),
            from_tag: caller.tag(),
            to_uri: self.to_uri.clone().unwrap_or_else(|| peer.uri.clone()),
            to_tag: None,
            cseq: 1,
            via: Some(caller.via()),
            contact: Some(caller.contact()),
            max_forwards: Some(70),
            body: self.body.clone().unwrap_or_default(),
            content_type: self.content_type.clone(),
            extra_headers: self.extra_headers.clone(),
        };
        let method = self.method;

        // At most ONE authenticated resend.
        let mut auth_retries_left: u8 = if responder.is_some() { 1 } else { 0 };
        loop {
            let req = generate_out_of_dialog_request(method, &opts);
            caller.try_send(&SipMessage::Request(req), wire_dst).await?;
            // Raw-receive so a 401/407 keeps its challenge header (a real digest
            // responder reads `nonce`/`realm` off it); a matching final returns
            // straight away, an unsolicited 100 is absorbed.
            let resp = recv_response_raw(&caller).await?;
            if resp.status == expect {
                return Ok(resp);
            }
            let is_challenge = matches!(resp.status, 401 | 407);
            // Not a retriable challenge (or no retry budget): surface the
            // deviation exactly as `try_expect(expect)` would.
            if !(is_challenge && auth_retries_left > 0 && responder.is_some()) {
                return Err(StepError::WrongStatus {
                    who: caller.name.clone(),
                    expected: expect,
                    got: resp.status,
                    reason: resp.reason.clone(),
                });
            }
            let responder = responder.expect("guarded above");
            let challenge = parse_challenge(&resp).unwrap_or(crate::realcall::auth::Challenge {
                status: resp.status,
                header_value: String::new(),
            });
            // Responder declines → surface the challenge as a plain deviation.
            let Some(credential) =
                responder.respond(&challenge, method.as_str(), &request_uri)
            else {
                return Err(StepError::WrongStatus {
                    who: caller.name.clone(),
                    expected: expect,
                    got: resp.status,
                    reason: resp.reason.clone(),
                });
            };
            // A non-INVITE final needs no ACK (§17.1.2.2). Resend with the
            // credential, a bumped CSeq, and a fresh Via branch (a new
            // transaction, §22.2).
            opts.cseq += 1;
            opts.via = Some(caller.via());
            opts.extra_headers
                .retain(|h| !h.name.eq_ignore_ascii_case(challenge.credential_header()));
            opts.extra_headers.push(SipHeader {
                name: challenge.credential_header().to_string(),
                value: credential,
            });
            auth_retries_left -= 1;
        }
    }
}

/// What a response fed to [`ClientInvite::absorb_response`] means for the
/// INVITE transaction it belongs to — the reactor's caller-side dispatch. The
/// `status` payloads are for the reactor's diagnostics/phase decisions (P1).
#[allow(dead_code)]
pub(crate) enum InviteResponseFate {
    /// A provisional (learned into the early dialog); keep waiting.
    Provisional { status: u16 },
    /// A 2xx — the dialog is confirmed; the caller must now [`ClientInvite::ack`].
    Answered,
    /// A non-2xx final (auto-ACKed on arrival, §17.1.1.3); the INVITE failed.
    Failed { status: u16 },
}

/// UAC-side INVITE client transaction + the dialog it is establishing.
pub struct ClientInvite {
    agent: Agent,
    /// Where to send the ACK if no Contact was learned (shouldn't happen for a
    /// well-behaved 2xx, but keeps the harness robust).
    fallback_addr: SocketAddr,
    /// The wire destination the INVITE was actually sent to (the proxy/B2BUA when
    /// [`Invite::through`] was used, else the peer). A CANCEL for a still-pending
    /// INVITE must follow the SAME path (RFC 3261 §9.1), so it is retained here.
    wire_dst: SocketAddr,
    original_invite: SipRequest,
    dialog: StackDialog,
    /// Per-forked-early-dialog CSeq (keyed by the fork's To-tag), for the
    /// delayed-offer forking case (RFC 3261 §12.1.2 / §12.2.1.1): one INVITE
    /// creates several early dialogs that each carry an INDEPENDENT CSeq space
    /// seeded from the INVITE's CSeq, so both forks' first PRACKs are
    /// `INVITE_CSeq + 1`. Without this the single shared counter makes each fork's
    /// PRACK (and the later BYE) non-contiguous within its dialog — which the
    /// per-dialog RFC 3261 §12.2.1.1 audit (correctly) rejects. Empty until a
    /// `with_to_tag` request fork is addressed.
    fork_cseq: HashMap<String, u32>,
}

impl ClientInvite {
    /// Wait for and assert a response status. Learns the remote tag (from the
    /// first tagged response) and the remote target (from Contact), so the
    /// later ACK/BYE route and address correctly. Returns the response.
    ///
    /// **Txn-layer auto-ACK (RFC 3261 §17.1.1.3):** any non-2xx final this
    /// transaction surfaces (here and in the `try_*` siblings) is ACKed
    /// automatically on the INVITE's branch, completing the client transaction
    /// the way a real txn layer does. A test whose *subject* is the
    /// ACK-retransmission dance hand-rolls raw sends instead.
    ///
    /// Panicking veneer over [`try_expect`](ClientInvite::try_expect).
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        unwrap_step(self.try_expect(status).await)
    }

    /// THE expect core for this INVITE transaction: a wrong status / timeout /
    /// unexpected request becomes a [`StepError`] (the functional lane panics
    /// on it via [`expect`](ClientInvite::expect)). On success it learns the
    /// dialog state (remote tag / target / route set).
    pub async fn try_expect(&mut self, status: u16) -> Result<SipResponse, StepError> {
        let resp = try_expect_response(&self.agent, status, Some(&self.ack_ctx())).await?;
        self.learn_from_response(&resp);
        Ok(resp)
    }

    /// The §17.1.1.3 auto-ACK context for THIS INVITE transaction (tracks the
    /// auth-retried INVITE — `ack_and_resend_with_auth` re-points
    /// `original_invite`, so a final to the retried transaction matches).
    fn ack_ctx(&self) -> AckCtx<'_> {
        AckCtx {
            agent: &self.agent,
            invite: &self.original_invite,
            wire_dst: self.wire_dst,
        }
    }

    /// SIPp-`optional` semantics for the load lane (newkahneed-033 ask D):
    /// wait for the FINAL response `status`, absorbing — and collecting — any
    /// interleaved provisional (1xx), instead of erroring on it the way
    /// [`try_expect`](Self::try_expect) does. A multi-leg shape legitimately
    /// sees a relay-timing-dependent number of provisionals (the reroute
    /// caller gets one or two 180s depending on when the alternate leg rings),
    /// so a body says `try_expect_final(200)` and asserts the collected 1xx
    /// list only where the count IS deterministic. Every absorbed provisional
    /// still feeds the dialog bookkeeping (early To-tag, Contact, route set),
    /// exactly as a sequence of `try_expect(18x)` calls would; the final
    /// overrides per §13.2.2.4. A non-matching FINAL is still a
    /// [`StepError::WrongStatus`].
    ///
    /// NOTE (033 ask D2): under `--auto-retransmit` the mux absorbs a REPEATED
    /// byte-identical provisional as a retransmission before any body sees it
    /// — see the retransmit-engine notes in `loadgen::mux` (`CallTxns`); a
    /// "ring again" assertion belongs to the functional/e2e surface, not here.
    pub async fn try_expect_final(
        &mut self,
        status: u16,
    ) -> Result<(SipResponse, Vec<SipResponse>), StepError> {
        let mut provisionals = Vec::new();
        loop {
            match self.agent.try_recv().await? {
                SipMessage::Response(r) if r.status == 100 => continue,
                SipMessage::Response(r) if r.status < 200 => {
                    self.learn_from_response(&r);
                    provisionals.push(r);
                }
                SipMessage::Response(r) => {
                    // §17.1.1.3 txn-layer auto-ACK — matching or not, a non-2xx
                    // final to THIS INVITE completes its client transaction.
                    self.ack_ctx().ack_non_2xx(&r).await?;
                    if r.status != status {
                        return Err(StepError::WrongStatus {
                            who: self.agent.name.clone(),
                            expected: status,
                            got: r.status,
                            reason: r.reason.clone(),
                        });
                    }
                    self.learn_from_response(&r);
                    return Ok((r, provisionals));
                }
                SipMessage::Request(r) => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK (036 ask B)
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!("got a {} request, expected a {status} final", r.method),
                    });
                }
            }
        }
    }

    /// Receive the next response to this INVITE **without asserting its status**
    /// (an unsolicited `100 Trying` is still absorbed). Unlike
    /// [`try_expect`](Self::try_expect) this surfaces a `401`/`407` challenge as an
    /// `Ok(response)` the auth retry point can inspect, rather than collapsing it
    /// to `WrongStatus`. It does NOT learn dialog state (a non-2xx final seeds no
    /// dialog); the caller learns from a 2xx via [`try_expect`]. Like every
    /// receive on this transaction, a non-2xx final (a challenge, a shed 503,
    /// any reject) is auto-ACKed on arrival (§17.1.1.3) — the auth retry then
    /// resends under a FRESH branch, a new transaction.
    // Test-only scaffolding for the `ack_and_resend_with_auth` end-to-end unit
    // test: it reads a raw 401/407 with a BLOCKING receive. The production actor
    // caller never re-receives here — its reactor already surfaced the response
    // via `recv_any`, which it folds in with [`absorb_response`](Self::absorb_response)
    // (that path drives the live §22.2 retry). Hence `#[cfg(test)]`-only.
    #[cfg(test)]
    pub(crate) async fn try_recv_response(&mut self) -> Result<SipResponse, StepError> {
        loop {
            match self.agent.try_recv().await? {
                SipMessage::Response(r) if r.status == 100 => continue,
                SipMessage::Response(r) => {
                    self.ack_ctx().ack_non_2xx(&r).await?;
                    return Ok(r);
                }
                SipMessage::Request(r) => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK (036 ask B)
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!("got a {} request, expected a response", r.method),
                    });
                }
            }
        }
    }

    /// Fold an ALREADY-received response (surfaced via [`Agent::recv_any`]) into
    /// this INVITE transaction WITHOUT recv'ing again — the reactive-actor
    /// caller-side analogue of [`try_expect`](Self::try_expect), which owns its
    /// own receive. A provisional / 2xx updates the dialog bookkeeping (early
    /// tag, target, route set) exactly as `try_expect` would; a non-2xx final is
    /// auto-ACKed on its branch (§17.1.1.3), like every other receive on this
    /// transaction. On [`InviteResponseFate::Answered`] the caller then
    /// [`ack`](Self::ack)s to confirm the dialog. `100 Trying` is a provisional
    /// here (the reactor ignores it) — the actor never awaits a specific status.
    pub(crate) async fn absorb_response(
        &mut self,
        resp: &SipResponse,
    ) -> Result<InviteResponseFate, StepError> {
        if resp.status < 200 {
            self.learn_from_response(resp);
            return Ok(InviteResponseFate::Provisional { status: resp.status });
        }
        if (200..300).contains(&resp.status) {
            self.learn_from_response(resp);
            return Ok(InviteResponseFate::Answered);
        }
        // A non-2xx final completes the client transaction — auto-ACK it on the
        // INVITE branch (§17.1.1.3), matching the `try_expect` path.
        self.ack_ctx().ack_non_2xx(resp).await?;
        Ok(InviteResponseFate::Failed { status: resp.status })
    }

    /// Learn the remote tag / target / route set from a response — the dialog
    /// bookkeeping shared by [`expect`](Self::expect) and
    /// [`try_expect`](Self::try_expect).
    fn learn_from_response(&mut self, resp: &SipResponse) {
        // RFC 3261 §13.2.2.4 / §12.1: the 2xx to the INVITE establishes the
        // dialog, so its To-tag is THE confirmed remote tag — even when an
        // earlier provisional from a *different* fork (RFC 3261 §12.1.2) seeded
        // another. A provisional only seeds the (early) remote tag when none is
        // known yet; the final 2xx overrides it so the ACK and every subsequent
        // in-dialog request address the dialog the 2xx actually confirmed.
        let is_2xx_invite = (200..300).contains(&resp.status) && resp.cseq.method == "INVITE";
        if let Some(tag) = &resp.to.tag {
            if is_2xx_invite || self.dialog.remote_tag.is_empty() {
                self.dialog.remote_tag = tag.clone();
            }
        }
        if let Some(target) = first_contact_uri(resp) {
            self.dialog.remote_target = target;
        }
        // Build the dialog route set from the response's Record-Route, REVERSED
        // (UAC, RFC 3261 §12.1.2), once — so a later 200 doesn't re-seed it.
        if self.dialog.route_set.is_empty() {
            let rr = get_headers(&resp.headers, "record-route");
            if !rr.is_empty() {
                self.dialog.route_set = rr.iter().rev().map(|s| s.to_string()).collect();
            }
        }
    }

    /// The Request-URI this INVITE targets (its wire R-URI) — the request-line
    /// input a credential is computed over ([`ChallengeResponder::respond`]).
    pub fn ruri(&self) -> &str {
        &self.original_invite.uri
    }

    /// **RFC 3261 §22.2 authentication retry** — the ONE auth adapter point wired
    /// into the INVITE choreography (see [`crate::realcall::auth`]). Given the
    /// `401`/`407` `challenge` this INVITE just drew and a configured `responder`:
    ///
    /// 1. ACK the challenge response (RFC 3261 §17.1.1.3: a non-2xx final to an
    ///    INVITE MUST be ACKed to complete the client transaction) via
    ///    [`generate_ack_for_non_2xx`], echoing the INVITE's Via/Route;
    /// 2. ask the `responder` for the credential header value;
    /// 3. resend THIS INVITE with the credential header added, its CSeq bumped by
    ///    one, and a fresh Via branch (a new client transaction), and re-point
    ///    `self` (its `original_invite`/`cancel_handle`/`ack` targets) at the
    ///    retried transaction.
    ///
    /// Returns `Ok(true)` when it resent (the caller then awaits the retried
    /// transaction's response), `Ok(false)` when the responder DECLINED (no
    /// resend — the caller surfaces the original challenge as `status_401/407`),
    /// or `Err` on a transport failure. The default (no responder) never reaches
    /// here, so today's classification is unchanged.
    pub(crate) async fn ack_and_resend_with_auth(
        &mut self,
        challenge: &SipResponse,
        responder: &dyn ChallengeResponder,
    ) -> Result<bool, StepError> {
        // 1. The challenged INVITE transaction is already complete: the receive
        //    that surfaced the challenge auto-ACKed it (§17.1.1.3 — see
        //    [`try_recv_response`](Self::try_recv_response)).

        // 2. Ask the pluggable adapter for a credential. A missing/parsed-off
        //    challenge still fires the responder (a static fixture ignores it);
        //    `None` = decline → no retry.
        let parsed = parse_challenge(challenge).unwrap_or(crate::realcall::auth::Challenge {
            status: challenge.status,
            header_value: String::new(),
        });
        let method = self.original_invite.method.to_string();
        let Some(credential) = responder.respond(&parsed, &method, self.ruri()) else {
            return Ok(false);
        };

        // 3. Rebuild THIS INVITE as a new transaction: bump CSeq, fresh Via
        //    branch, add the credential header (RFC 3261 §22.2). Serialization is
        //    driven by the header list + first line + body, so rewriting the
        //    header vector (and re-parsing to keep the structured fields in step
        //    for the later ACK/CANCEL) is a faithful resend.
        let new_cseq = self.original_invite.cseq.seq + 1;
        let mut headers = self.original_invite.headers.clone();
        headers = set_header(&headers, "Via", &self.agent.via_header());
        headers = set_header(&headers, "CSeq", &format!("{new_cseq} {method}"));
        // Drop any prior credential of the same header (a second challenge round
        // would replace it) then add this one.
        headers.retain(|h| !h.name.eq_ignore_ascii_case(parsed.credential_header()));
        headers.push(SipHeader {
            name: parsed.credential_header().to_string(),
            value: credential,
        });

        let bytes = sip_message::serialize_request_parts(&self.original_invite, &headers);
        let resent = CustomParser::new().parse(&bytes).map_err(|e| StepError::Unparseable {
            who: self.agent.name.clone(),
            detail: format!("rebuilt authed INVITE did not parse: {e}"),
        })?;
        let SipMessage::Request(resent) = resent else {
            return Err(StepError::UnexpectedKind {
                who: self.agent.name.clone(),
                detail: "rebuilt authed INVITE parsed as a response".to_string(),
            });
        };

        self.agent.try_send(&SipMessage::Request(resent.clone()), self.wire_dst).await?;
        // Re-point the transaction state at the retried INVITE: the CANCEL / ACK /
        // dialog CSeq must all follow the new transaction, not the challenged one.
        self.original_invite = resent;
        self.dialog.local_cseq = new_cseq;
        Ok(true)
    }

    /// A cheap, `Send + 'static` handle that can CANCEL this still-pending INVITE
    /// later — the load driver registers it in its teardown scope so a call that
    /// fails *before* confirmation is CANCELled (RFC 3261 §9.1), never leaked on
    /// the SUT. Holds its own [`Agent`] clone (shared `Arc` endpoint), so it
    /// works even after the scenario's own handles are dropped.
    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle {
            agent: self.agent.clone(),
            wire_dst: self.wire_dst,
            original_invite: self.original_invite.clone(),
        }
    }

    /// Send a CANCEL for this still-pending INVITE (RFC 3261 §9.1). The CANCEL
    /// reuses the INVITE's Request-URI / Call-ID / From / To / topmost Via branch
    /// and the INVITE's CSeq *number* with method `CANCEL`, and is sent to the
    /// SAME wire destination the INVITE took (the proxy / B2BUA when
    /// [`Invite::through`] was used). Returns a client transaction so the caller
    /// can `expect` the `200 OK` to the CANCEL; the matching `487 Request
    /// Terminated` for the INVITE arrives on this same UA and is consumed via
    /// [`ClientInvite::expect`].
    pub async fn cancel(&self) -> InDialogTxn {
        unwrap_step(try_send_cancel(&self.agent, &self.original_invite, self.wire_dst).await);
        InDialogTxn {
            agent: self.agent.clone(),
            // A CANCEL transaction's finals take no ACK; the INVITE's 487 is
            // read — and auto-ACKed — via [`ClientInvite::expect`].
            invite: None,
            wire_dst: self.wire_dst,
        }
    }

    /// Begin an in-dialog request on the *early* dialog (before the final 2xx /
    /// ACK) — the PRACK path (RFC 3262): alice PRACKs a reliable 183 while the
    /// INVITE transaction is still pending. The CSeq advances on the shared
    /// dialog state, so the later BYE numbers correctly.
    pub fn send_request(&mut self, method: InDialogMethod) -> InDialogRequest<'_> {
        InDialogRequest::new(self.agent.clone(), &mut self.dialog, self.fallback_addr, method)
            .with_fork_cseq(&mut self.fork_cseq)
    }

    /// PRACK the reliable provisional `reliable_1xx` (RFC 3262 §7.2), fallibly:
    /// builds the `RAck` (`<RSeq> <CSeq-num> <CSeq-method>`) from the response's
    /// own RSeq + CSeq and sends the PRACK on the early dialog. Returns the PRACK
    /// client transaction to [`try_expect(200)`](InDialogTxn::try_expect) on. A
    /// response with no (or an unparseable) `RSeq` is not PRACK-able — that
    /// surfaces as [`StepError::UnexpectedKind`], never a panic.
    pub async fn try_prack(
        &mut self,
        reliable_1xx: &SipResponse,
    ) -> Result<InDialogTxn, StepError> {
        let rack = rack_for(reliable_1xx).ok_or_else(|| StepError::UnexpectedKind {
            who: self.agent.name.clone(),
            detail: format!(
                "cannot PRACK the {} {}: no parseable RSeq header (not a reliable provisional)",
                reliable_1xx.status, reliable_1xx.reason
            ),
        })?;
        self.send_request(InDialogMethod::Prack).with_rack(&rack).try_send().await
    }

    /// [`try_prack`](Self::try_prack) that also returns the PRACK request as
    /// sent — the reactive actor keys its "PRACK awaiting 200" ledger obligation
    /// on the returned request's CSeq (the 200 carries the same number). Same
    /// RAck derivation; the linear lane uses the request-less [`try_prack`].
    pub async fn try_prack_with_request(
        &mut self,
        reliable_1xx: &SipResponse,
    ) -> Result<(InDialogTxn, SipRequest), StepError> {
        let rack = rack_for(reliable_1xx).ok_or_else(|| StepError::UnexpectedKind {
            who: self.agent.name.clone(),
            detail: format!(
                "cannot PRACK the {} {}: no parseable RSeq header (not a reliable provisional)",
                reliable_1xx.status, reliable_1xx.reason
            ),
        })?;
        self.send_request(InDialogMethod::Prack).with_rack(&rack).try_send_with_request().await
    }

    /// Generate and send the ACK for the 2xx (CSeq reused from the INVITE per
    /// RFC 3261 §13.2.2.4), then return the confirmed [`Dialog`]. With a route
    /// set the ACK carries Route headers and goes to the first hop (the proxy).
    pub async fn ack(&mut self) -> Dialog {
        self.ack_with(None).await
    }

    /// ACK the 2xx carrying an optional SDP body — the delayed-offer answer
    /// rides the ACK when the 200 OK carried the offer (RFC 3264 §4).
    pub async fn ack_with(&mut self, sdp: Option<&str>) -> Dialog {
        let handle = InviteClientTransactionHandle {
            original_invite: self.original_invite.clone(),
        };
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(Some(&handle), &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(ack), dst).await;
        // If this dialog confirmed to a fork that carried its own PRACK sequence,
        // continue from THAT fork's CSeq so the post-confirm BYE/re-INVITE is
        // contiguous within the winning dialog (RFC 3261 §12.2.1.1), not a reuse
        // of a CSeq the fork already spent.
        let mut confirmed = self.dialog.clone();
        if let Some(&fork) = self.fork_cseq.get(&confirmed.remote_tag) {
            confirmed.local_cseq = confirmed.local_cseq.max(fork);
        }
        Dialog {
            agent: self.agent.clone(),
            fallback_addr: dst,
            dialog: confirmed,
        }
    }
}

/// A `Send + 'static` CANCEL handle for a still-pending INVITE (see
/// [`ClientInvite::cancel_handle`]). The load driver's teardown scope holds one
/// for any call still in its early phase, so a failed call releases the SUT.
#[derive(Clone)]
pub struct CancelHandle {
    agent: Agent,
    wire_dst: SocketAddr,
    original_invite: SipRequest,
}

impl CancelHandle {
    /// Send a CANCEL for the pending INVITE (RFC 3261 §9.1) on a best-effort
    /// basis — a transport error is swallowed (the call is already failing). Does
    /// not wait for the 200/487.
    pub async fn cancel_best_effort(&self) {
        let _ = try_send_cancel(&self.agent, &self.original_invite, self.wire_dst).await;
    }
}

// ---------------------------------------------------------------------------
// Confirmed dialog (in-dialog requests)
// ---------------------------------------------------------------------------

/// A confirmed dialog. In-dialog requests auto-increment CSeq and route to the
/// remote target. `Clone` so the load driver can snapshot it into its teardown
/// scope (each clone shares the `Arc` endpoint and carries the dialog state
/// needed to BYE).
#[derive(Clone)]
pub struct Dialog {
    agent: Agent,
    fallback_addr: SocketAddr,
    dialog: StackDialog,
}

impl Dialog {
    /// Set the dialog's local CSeq floor — the next in-dialog request uses
    /// `v + 1`. RFC 3261 §12.2.1.1 lets a UA pick ANY initial sequence number
    /// for its own CSeq space, so a test can deliberately align this side's
    /// CSeq with the peer's to force `(Call-ID, CSeq, method)`-coincident
    /// crossing transactions (e.g. BYE/BYE glare through a relay, where only
    /// the top-Via branch disambiguates the two 200s — RFC 3261 §17.1.3).
    pub fn set_local_cseq(&mut self, v: u32) {
        self.dialog.local_cseq = v;
    }

    /// This side's current CSeq high-water — the number the LAST in-dialog
    /// request this UA sent used (0 before any). The reactive actor reads it
    /// right after [`bye`](Self::bye) / [`request`](Self::request) to key the
    /// ledger obligation the sent request opens (the matching final response
    /// carries the same `CSeq` number).
    pub fn local_cseq(&self) -> u32 {
        self.dialog.local_cseq
    }

    /// This side's dialog tag (the To-tag a UAS minted on its 2xx / the
    /// From-tag a UAC sent). Read-only; lets a test re-answer an INVITE
    /// retransmission with the SAME tag (a faithful RFC 3261 §17.2.1 2xx
    /// retransmission via `Respond::with_to_tag`) instead of minting a
    /// phantom fork dialog.
    pub fn local_tag(&self) -> &str {
        &self.dialog.local_tag
    }

    /// Send a BYE (CSeq auto-incremented). Returns its client transaction.
    pub async fn bye(&mut self) -> InDialogTxn {
        self.request(InDialogMethod::Bye, None).await
    }

    /// Best-effort BYE for the load driver's teardown: builds and sends the BYE
    /// (advancing the dialog CSeq so it is valid against the SUT), swallowing any
    /// transport error and **not** waiting for the 200. Runs on a failed call to
    /// release the dialog on the SUT (RFC 3261 §15) so no call is leaked.
    pub async fn bye_best_effort(&mut self) {
        let _ = self.send_request(InDialogMethod::Bye).try_send().await;
    }

    /// ACK a re-INVITE's 2xx on this confirmed dialog (RFC 3261 §13.2.2.4 — the
    /// ACK echoes the re-INVITE's CSeq, which `request(INVITE, …)` left as the
    /// dialog's `local_cseq`). Carries an optional SDP answer (the delayed-offer
    /// case where the answer rides the ACK, RFC 3264 §4). Routed to the next hop
    /// like any in-dialog request; the B2BUA relays it end-to-end.
    pub async fn ack(&mut self, sdp: Option<&str>) {
        self.ack_for(self.dialog.local_cseq, sdp).await;
    }

    /// ACK a re-INVITE's 2xx echoing an **explicit** INVITE CSeq (RFC 3261
    /// §13.2.2.4 — the ACK number is the re-INVITE's, taken from the 2xx being
    /// ACKed, NOT this dialog's current `local_cseq`, which may have advanced past
    /// it if another in-dialog request went out meanwhile). Idempotent and
    /// re-derivable from the confirmed dialog + the response's CSeq, so a
    /// retransmitted 2xx can always be re-ACKed with no per-call one-shot state
    /// (mirrors the loadgen mux's `(Call-ID, CSeq)` re-ACK contract). Carries an
    /// optional SDP answer (the delayed-offer case, RFC 3264 §4).
    pub async fn ack_for(&mut self, invite_cseq: u32, sdp: Option<&str>) {
        let opts = GenerateAckFor2xxOpts {
            via: Some(self.agent.via()),
            cseq: Some(invite_cseq),
            body: sdp.map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            ..Default::default()
        };
        let ack = generate_ack_for_2xx(None, &self.dialog, &opts);
        let dst = next_hop(&self.dialog, self.fallback_addr);
        self.agent.send(&SipMessage::Request(ack), dst).await;
    }

    /// Send any in-dialog request (re-INVITE, INFO, …); attach an SDP body
    /// with `sdp`. Sugar over the [`send_request`](Dialog::send_request)
    /// builder — same mechanics (CSeq bump, next-hop routing, §17.1.1.3
    /// (re-)INVITE retention on the returned transaction).
    pub async fn request(&mut self, method: InDialogMethod, sdp: Option<&str>) -> InDialogTxn {
        let mut req = self.send_request(method);
        if let Some(s) = sdp {
            req = req.with_sdp(s);
        }
        req.send().await
    }

    /// Begin an in-dialog request with fine-grained control over RAck (RFC 3262
    /// PRACK) and arbitrary extra headers. Returns a builder; call
    /// [`InDialogRequest::send`]. Use this over [`request`](Dialog::request) when
    /// the request needs an `RAck` header (PRACK) or other custom headers.
    pub fn send_request(&mut self, method: InDialogMethod) -> InDialogRequest<'_> {
        InDialogRequest::new(self.agent.clone(), &mut self.dialog, self.fallback_addr, method)
    }

    /// Send an in-dialog **re-INVITE** (optional SDP offer) and keep the
    /// transaction handle, so the renegotiation can later be CANCELled — the
    /// in-dialog mirror of [`ClientInvite::cancel`] (RFC 3261 §9.1). Mechanics
    /// are identical to [`request`](Dialog::request)`(InDialogMethod::Invite, …)`
    /// (same CSeq bump on this dialog, same next-hop routing); only the
    /// returned handle differs: [`ClientReinvite`] can `expect(…)` responses
    /// *and* [`cancel`](ClientReinvite::cancel) the still-pending re-INVITE.
    pub async fn reinvite(&mut self, sdp: Option<&str>) -> ClientReinvite {
        let mut builder = self.send_request(InDialogMethod::Invite);
        if let Some(s) = sdp {
            builder = builder.with_sdp(s);
        }
        let (_txn, request) = unwrap_step(builder.try_send_with_request().await);
        ClientReinvite {
            agent: self.agent.clone(),
            wire_dst: next_hop(&self.dialog, self.fallback_addr),
            original_invite: request,
        }
    }
}

/// Client transaction for an in-dialog re-INVITE sent via [`Dialog::reinvite`].
/// Like [`InDialogTxn`] it can await responses; additionally it holds the
/// re-INVITE as sent (plus its wire destination), so the pending renegotiation
/// can be CANCELled (RFC 3261 §9.1) — the in-dialog counterpart of
/// [`ClientInvite::cancel`].
pub struct ClientReinvite {
    agent: Agent,
    wire_dst: SocketAddr,
    original_invite: SipRequest,
}

impl ClientReinvite {
    /// The §17.1.1.3 auto-ACK context for this re-INVITE transaction.
    fn ack_ctx(&self) -> AckCtx<'_> {
        AckCtx {
            agent: &self.agent,
            invite: &self.original_invite,
            wire_dst: self.wire_dst,
        }
    }

    /// Wait for and assert a response status (the relayed 1xx/2xx/487 for this
    /// re-INVITE, or the 200 to a CANCEL — whichever arrives next). A non-2xx
    /// final to the re-INVITE is auto-ACKed on its branch (RFC 3261
    /// §17.1.1.3), like [`ClientInvite::expect`]. Panicking veneer over
    /// [`try_expect`](ClientReinvite::try_expect).
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        unwrap_step(self.try_expect(status).await)
    }

    /// Fallible [`expect`](ClientReinvite::expect).
    pub async fn try_expect(&mut self, status: u16) -> Result<SipResponse, StepError> {
        try_expect_response(&self.agent, status, Some(&self.ack_ctx())).await
    }

    /// CANCEL this still-pending re-INVITE (RFC 3261 §9.1) — the in-dialog
    /// mirror of [`ClientInvite::cancel`]. The CANCEL reuses the re-INVITE's
    /// Request-URI / Call-ID / From / To / topmost Via branch, echoes its Route
    /// set, uses the re-INVITE's CSeq *number* with method `CANCEL`, and goes
    /// to the SAME wire destination the re-INVITE took. Returns a client
    /// transaction to `expect(200)` on; the matching `487 Request Terminated`
    /// for the re-INVITE arrives on this same UA via
    /// [`ClientReinvite::expect`]. Per §9, this ends only the renegotiation —
    /// the established dialog (and the call through a B2BUA) must survive.
    pub async fn cancel(&self) -> InDialogTxn {
        unwrap_step(try_send_cancel(&self.agent, &self.original_invite, self.wire_dst).await);
        InDialogTxn {
            agent: self.agent.clone(),
            // The CANCELled re-INVITE's 487 is read — and auto-ACKed — via
            // [`ClientReinvite::expect`]; the CANCEL's own finals take no ACK.
            invite: None,
            wire_dst: self.wire_dst,
        }
    }
}

/// Builder for an in-dialog request carrying an `RAck` and/or custom headers
/// (the PRACK path, RFC 3262 §7.2). Borrows the originating dialog's
/// [`StackDialog`] so the CSeq bump persists — works over both a confirmed
/// [`Dialog`] and an early [`ClientInvite`] (PRACK precedes the final 2xx/ACK).
pub struct InDialogRequest<'a> {
    agent: Agent,
    dialog: &'a mut StackDialog,
    fallback: SocketAddr,
    method: InDialogMethod,
    body: Vec<u8>,
    content_type: Option<String>,
    rack: Option<String>,
    extra_headers: Vec<SipHeader>,
    to_tag: Option<String>,
    /// Per-fork CSeq map (see [`ClientInvite::fork_cseq`]). Present only on the
    /// early-dialog path; when a `with_to_tag` fork is addressed the CSeq comes
    /// from this independent per-fork sequence, not the shared dialog counter.
    fork_cseq: Option<&'a mut HashMap<String, u32>>,
}

impl<'a> InDialogRequest<'a> {
    fn new(agent: Agent, dialog: &'a mut StackDialog, fallback: SocketAddr, method: InDialogMethod) -> Self {
        InDialogRequest {
            agent,
            dialog,
            fallback,
            method,
            body: vec![],
            content_type: None,
            rack: None,
            extra_headers: vec![],
            to_tag: None,
            fork_cseq: None,
        }
    }

    /// Wire in the originating [`ClientInvite`]'s per-fork CSeq map so a
    /// `with_to_tag` request uses that fork's independent sequence.
    fn with_fork_cseq(mut self, map: &'a mut HashMap<String, u32>) -> Self {
        self.fork_cseq = Some(map);
        self
    }

    /// Address this request to a specific early dialog by overriding the remote
    /// (To) tag — the per-fork PRACK in `prack-forking` (RFC 3262 §5). The shared
    /// CSeq counter still advances; the slim harness does not assert per-fork CSeq
    /// independence (the B2BUA recomputes the outbound CSeq per dialog anyway).
    pub fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_string());
        self
    }

    /// Attach an SDP body (e.g. the answer carried in a PRACK to a delayed
    /// offer, RFC 3264 §4). Thin sugar over [`with_body`](Self::with_body) that
    /// pins `Content-Type: application/sdp`.
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.body = sdp.as_bytes().to_vec();
        self.content_type = Some("application/sdp".to_string());
        self
    }

    /// Attach an arbitrary-MIME body — mirror of [`with_sdp`](Self::with_sdp)
    /// for any `Content-Type` and raw (binary-safe) bytes. Drives an in-dialog
    /// `INFO`/other with a real payload: an `application/orangeindata` SUP body,
    /// a `multipart/mixed` dual-part body, a `User-To-User` INFO, etc. With
    /// non-empty `bytes` the request carries `Content-Type` + `Content-Length`;
    /// an empty `bytes` emits no `Content-Type` (the generator only stamps it for
    /// a non-empty body) — for a bodyless typed header use `with_header`.
    pub fn with_body(mut self, content_type: &str, bytes: Vec<u8>) -> Self {
        self.body = bytes;
        self.content_type = Some(content_type.to_string());
        self
    }

    /// Set the `RAck` header (`<rseq> <cseq> <method>`, RFC 3262 §7.2).
    pub fn with_rack(mut self, rack: &str) -> Self {
        self.rack = Some(rack.to_string());
        self
    }

    /// Attach an arbitrary extra header.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Generate and send the request; returns its client transaction. Panics on
    /// a transport failure — use [`try_send`](Self::try_send) in the load lane.
    pub async fn send(self) -> InDialogTxn {
        unwrap_step(self.try_send().await)
    }

    /// Fallible [`send`](Self::send) — the generic any-method in-dialog send for
    /// the load lane: a transport failure surfaces as [`StepError::Transport`]
    /// instead of a panic. The mechanical layer (Via/branch, CSeq bump, tags,
    /// route set) is identical.
    pub async fn try_send(mut self) -> Result<InDialogTxn, StepError> {
        self.try_send_inner().await.map(|(txn, _)| txn)
    }

    /// [`try_send`](Self::try_send) that also returns the request as sent —
    /// for tagging a message ANCHOR on a request no test agent receives (the
    /// REFER whose receiver is the SUT; see `CallCtx::anchor_sent`). The common
    /// path pays nothing for it (`try_send` discards the clone-free original).
    pub async fn try_send_with_request(mut self) -> Result<(InDialogTxn, SipRequest), StepError> {
        self.try_send_inner().await
    }

    async fn try_send_inner(&mut self) -> Result<(InDialogTxn, SipRequest), StepError> {
        let opts = GenerateInDialogRequestOpts {
            via: Some(self.agent.via()),
            contact: Some(self.agent.contact()),
            body: std::mem::take(&mut self.body),
            content_type: self.content_type.take(),
            rack: self.rack.take(),
            extra_headers: std::mem::take(&mut self.extra_headers),
            ..Default::default()
        };
        // Per-fork addressing: generate against a dialog view with the chosen
        // remote tag. For a forked early dialog (a `with_to_tag` other than the
        // shared dialog's own tag) the CSeq rides that fork's OWN sequence
        // (seeded from the INVITE's CSeq), not the shared counter — RFC 3261
        // §12.2.1.1: each early dialog increments independently.
        let mut view = self.dialog.clone();
        let mut opts = opts;
        // On the early-dialog path EVERY explicit `with_to_tag` addresses a
        // distinct forked early dialog (RFC 3262 §5), so it rides that fork's OWN
        // CSeq sequence (seeded from the INVITE's CSeq) — NOT the shared counter,
        // which a sibling fork's PRACK must not perturb.
        let forked = self.to_tag.is_some() && self.fork_cseq.is_some();
        if let (Some(tag), Some(map)) = (self.to_tag.as_ref(), self.fork_cseq.as_deref_mut()) {
            view.remote_tag = tag.clone();
            // Seed from the INVITE's CSeq (the dialog's current local_cseq) the
            // first time this fork is addressed, then advance by one per request.
            let entry = map.entry(tag.clone()).or_insert(self.dialog.local_cseq);
            *entry += 1;
            opts.cseq = Some(*entry);
        } else if let Some(t) = &self.to_tag {
            view.remote_tag = t.clone();
        }
        let res = generate_in_dialog_request(self.method, &view, &opts);
        // Advance the SHARED dialog counter only for a non-forked request (a
        // forked request advanced its own per-fork entry above and must leave the
        // shared sequence untouched).
        if !forked {
            self.dialog.local_cseq = res.dialog.local_cseq;
        }
        let dst = next_hop(self.dialog, self.fallback);
        // Send the wrapped request, then hand the original back (no clone).
        let msg = SipMessage::Request(res.request);
        self.agent.try_send(&msg, dst).await?;
        let SipMessage::Request(request) = msg else { unreachable!() };
        Ok((
            InDialogTxn {
                agent: self.agent.clone(),
                // A re-INVITE's non-2xx final takes a txn-layer ACK
                // (§17.1.1.3) — retain the request so the txn can build it.
                invite: matches!(self.method, InDialogMethod::Invite).then(|| request.clone()),
                wire_dst: dst,
            },
            request,
        ))
    }
}

/// Client transaction for an in-dialog request.
///
/// For a **re-INVITE** it retains the request as sent (plus its wire
/// destination), so a 3xx–6xx final — a 488 to a rejected renegotiation, a 491
/// glare — is auto-ACKed on the re-INVITE's branch/CSeq (RFC 3261 §17.1.1.3),
/// exactly like [`ClientInvite`]'s finals (newkahneed-034 ask B). Non-INVITE
/// transactions (BYE, INFO, CANCEL, …) retain nothing: their finals take no ACK.
pub struct InDialogTxn {
    agent: Agent,
    /// The sent request, retained ONLY when it was an (re-)INVITE — the
    /// §17.1.1.3 non-2xx auto-ACK needs its Via branch / CSeq / R-URI / Route.
    invite: Option<SipRequest>,
    /// Where the request was sent (the next hop) — the non-2xx ACK is
    /// hop-by-hop and follows the SAME path.
    wire_dst: SocketAddr,
}

impl InDialogTxn {
    /// The §17.1.1.3 auto-ACK context — `None` for a non-INVITE transaction.
    fn ack_ctx(&self) -> Option<AckCtx<'_>> {
        self.invite.as_ref().map(|invite| AckCtx {
            agent: &self.agent,
            invite,
            wire_dst: self.wire_dst,
        })
    }

    /// Wait for and assert a response status. Panicking veneer over
    /// [`try_expect`](InDialogTxn::try_expect).
    pub async fn expect(&mut self, status: u16) -> SipResponse {
        unwrap_step(self.try_expect(status).await)
    }

    /// THE expect core for this transaction: a wrong status / timeout /
    /// unexpected request becomes a [`StepError`] (the functional lane panics
    /// on it via [`expect`](InDialogTxn::expect)).
    pub async fn try_expect(&mut self, status: u16) -> Result<SipResponse, StepError> {
        try_expect_response(&self.agent, status, self.ack_ctx().as_ref()).await
    }

    /// Fallible, tolerant [`try_expect`](InDialogTxn::try_expect): while waiting
    /// for the response, 200-OK any inbound request whose method is in
    /// `tolerate` (e.g. a `NOTIFY` that races ahead of the REFER's 202 on the
    /// same socket) and keep waiting — instead of mis-classifying the reorder.
    /// A real load tool faces UDP reordering, so this is the production-correct
    /// behaviour, not just a fake-fabric workaround.
    pub async fn try_expect_tolerating(
        &mut self,
        status: u16,
        tolerate: &[&str],
    ) -> Result<SipResponse, StepError> {
        try_expect_response_tolerating(&self.agent, status, tolerate, self.ack_ctx().as_ref())
            .await
    }

    /// Like [`expect`](InDialogTxn::expect), but first drains (and 200-OKs) any
    /// inbound requests whose method is in `tolerate` — the response-side analog
    /// of [`Agent::receive_tolerating`]. Under a paused clock a keepalive OPTIONS
    /// retransmit can race the awaited response on the same socket; tolerate it
    /// rather than relax the assertion (CLAUDE.md retransmit hazard).
    /// Panicking veneer over [`try_expect_tolerating`](InDialogTxn::try_expect_tolerating).
    pub async fn expect_tolerating(&mut self, status: u16, tolerate: &[&str]) -> SipResponse {
        unwrap_step(self.try_expect_tolerating(status, tolerate).await)
    }
}

// ---------------------------------------------------------------------------
// UAS-side server transaction
// ---------------------------------------------------------------------------

/// UAS-side transaction for a received request. `respond` echoes Via/From/To/
/// Call-ID/CSeq and mints a stable To-tag on the first non-100 response.
pub struct ServerTxn {
    agent: Agent,
    request: SipRequest,
    to_tag: Option<String>,
    route_set: Vec<String>,
}

impl ServerTxn {
    /// The ONE constructor from a received request. Captures the UAS route set
    /// (RFC 3261 §12.1.1): the request's Record-Route rows in received order —
    /// used if this UAS later originates in-dialog requests (e.g. bob sends
    /// the BYE).
    fn from_request(agent: Agent, request: SipRequest) -> Self {
        let route_set = get_headers(&request.headers, "record-route")
            .iter()
            .map(|s| s.to_string())
            .collect();
        ServerTxn { agent, request, to_tag: None, route_set }
    }

    /// The received request (for inspecting headers / SDP).
    pub fn request(&self) -> &SipRequest {
        &self.request
    }

    /// Send a response. Returns a builder for attaching an SDP answer and/or
    /// custom headers (e.g. `Require: 100rel` + `RSeq` on a reliable 18x).
    pub fn respond(&mut self, status: u16, reason: &str) -> Respond<'_> {
        Respond {
            txn: self,
            status,
            reason: reason.to_string(),
            sdp: None,
            extra_headers: vec![],
            to_tag: None,
        }
    }

    /// Assert the §17.1.1.3 hop ACK for THIS transaction's non-2xx final
    /// (036 ask B). Returns immediately if the transaction layer already
    /// claimed it (it may have landed before or after any of the body's other
    /// receives — matching is by `(Call-ID, INVITE branch)`, never positional);
    /// otherwise pulls until the ACK arrives, the panicking-veneer sibling of
    /// [`try_expect_ack`](Self::try_expect_ack). Purely optional: an unread ACK
    /// is still recorded at delivery (036 ask A) and the gating
    /// `unackedInviteNon2xxFinal` wire rule settles the obligation at
    /// `finish()` — reach for this when the test asserts the ACK at a specific
    /// point in the flow.
    pub async fn expect_ack(&self) {
        unwrap_step(self.try_expect_ack().await)
    }

    /// Fallible core of [`expect_ack`](Self::expect_ack).
    pub async fn try_expect_ack(&self) -> Result<(), StepError> {
        let Some(branch) = top_via_branch(&self.request.headers) else {
            return Err(StepError::UnexpectedKind {
                who: self.agent.name.clone(),
                detail: "expect_ack on a request with no top-Via branch".to_string(),
            });
        };
        loop {
            if self.agent.acks.is_fulfilled(&self.request.call_id, &branch) {
                return Ok(());
            }
            // Pull; the receive core sights (and thereby fulfils) a matching
            // ACK. Anything else arriving while the test explicitly awaits the
            // ACK is a deviation, exactly as `receive("ACK")` would treat it.
            match self.agent.try_recv().await? {
                SipMessage::Request(r) if r.method.as_str() == "ACK" => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // ours (loop returns Ok) or another txn's obligation
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: "got an ACK for a different transaction".to_string(),
                    });
                }
                SipMessage::Request(r) => {
                    return Err(StepError::WrongMethod {
                        who: self.agent.name.clone(),
                        expected: "ACK".to_string(),
                        got: r.method.to_string(),
                    })
                }
                SipMessage::Response(r) => {
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!(
                            "got a {} {} response, expected the ACK to this txn's final",
                            r.status, r.reason
                        ),
                    })
                }
            }
        }
    }

    /// Form the UAS-side confirmed [`Dialog`] for this transaction, so this UA
    /// can originate in-dialog requests (e.g. the callee sends the BYE). Call
    /// after responding 2xx (so the To-tag is minted). The remote target is the
    /// caller's Contact; the route set is the request's Record-Route in order
    /// (§12.1.1), so in-dialog requests route back through any proxy.
    pub fn dialog(&self) -> Dialog {
        let req = &self.request;
        let local_tag = self.to_tag.clone().unwrap_or_default();
        let remote_target = get_header(&req.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_else(|| req.from.uri.clone());
        let dialog = StackDialog {
            call_id: req.call_id.clone(),
            local_tag,
            remote_tag: req.from.tag.clone().unwrap_or_default(),
            // From the UAS's view, "local" is itself and "remote" is the caller.
            // RFC 3261 §12.1.1: the dialog LOCAL URI is the To field of the
            // request, NOT the agent's own AOR — they coincide in the usual
            // alice→bob case (To:bob == bob's uri) but diverge when a UAS is
            // handed an INVITE addressed to a third party (e.g. the MRF media
            // leg carries To:dest): the callee's in-dialog requests must then
            // carry From:dest, and the recorded-trace midDialogUri audit — which
            // now merges both tag orientations into one dialog slice — checks it.
            local_uri: req.to.uri.clone(),
            remote_uri: req.from.uri.clone(),
            remote_target,
            local_cseq: 0, // UAS originates its own CSeq space; first request → 1
            route_set: self.route_set.clone(),
        };
        let fallback = next_hop(&dialog, top_via_addr(req).unwrap_or(self.agent.addr));
        Dialog {
            agent: self.agent.clone(),
            fallback_addr: fallback,
            dialog,
        }
    }
}

/// Builder for a UAS response (lets an SDP answer and custom headers be
/// attached fluently).
pub struct Respond<'a> {
    txn: &'a mut ServerTxn,
    status: u16,
    reason: String,
    sdp: Option<String>,
    extra_headers: Vec<SipHeader>,
    to_tag: Option<String>,
}

impl<'a> Respond<'a> {
    pub fn with_sdp(mut self, sdp: &str) -> Self {
        self.sdp = Some(sdp.to_string());
        self
    }

    /// Force a specific To-tag on this response instead of the auto-minted one.
    /// Used to simulate a forking endpoint emitting several early dialogs with
    /// distinct tags (RFC 3261 §12.1; the per-fork 18x in `prack-forking`).
    pub fn with_to_tag(mut self, tag: &str) -> Self {
        self.to_tag = Some(tag.to_string());
        self
    }

    /// Attach a custom header (e.g. `Require: 100rel`, `RSeq: 1` on a reliable
    /// provisional, RFC 3262). Repeatable; order is preserved.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.extra_headers.push(SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        });
        self
    }

    /// Mark this provisional RELIABLE (RFC 3262 §3): stamps `Require: 100rel` +
    /// `RSeq: <rseq>` so the peer must PRACK it. Only meaningful on a 101–199
    /// response to a dialog-creating INVITE whose sender opted in
    /// (`Supported`/`Require: 100rel`).
    pub fn reliable(self, rseq: u32) -> Self {
        self.with_header("Require", "100rel").with_header("RSeq", &rseq.to_string())
    }

    /// Generate and send the response. Panics on a transport failure — use
    /// [`try_send`](Self::try_send) in the load lane.
    pub async fn send(self) {
        unwrap_step(self.try_send().await)
    }

    /// Fallible [`send`](Self::send) for the load lane: a transport failure
    /// surfaces as [`StepError::Transport`] instead of a panic.
    pub async fn try_send(self) -> Result<(), StepError> {
        let txn = self.txn;
        // An explicit per-fork To-tag overrides (and does not disturb) the txn's
        // sticky auto-minted tag, so distinct early dialogs keep distinct tags.
        let to_tag = if let Some(t) = self.to_tag {
            Some(t)
        } else {
            if self.status > 100 && txn.to_tag.is_none() {
                txn.to_tag = Some(txn.agent.tag());
            }
            txn.to_tag.clone()
        };
        // Contact is required on 2xx and useful on 18x to establish the early
        // dialog's remote target; omit on plain 100.
        let contact = if self.status >= 180 {
            Some(txn.agent.contact())
        } else {
            None
        };
        // A conformant UAS lists its methods/extensions on a 2xx INVITE
        // (RFC 3261 §13.2.1 SHOULD Allow, §20.37 Supported) so the peer can
        // negotiate re-INVITE/UPDATE/PRACK. The test UA answers anything, so it
        // omitted these — add them (unless the fixture already supplied one) so
        // the UA is RFC-compliant, matching the live SIPp endpoints.
        let mut extra_headers = self.extra_headers.clone();
        if (200..300).contains(&self.status) && txn.request.cseq.method.as_str() == "INVITE" {
            let has_allow = extra_headers.iter().any(|h| h.name.eq_ignore_ascii_case("Allow"));
            let has_supported =
                extra_headers.iter().any(|h| h.name.eq_ignore_ascii_case("Supported"));
            if !has_allow {
                extra_headers.push(SipHeader { name: "Allow".into(), value: B2BUA_ALLOW.into() });
            }
            if !has_supported {
                extra_headers
                    .push(SipHeader { name: "Supported".into(), value: B2BUA_SUPPORTED.into() });
            }
        }
        let opts = GenerateResponseOpts {
            to_tag,
            contact,
            body: self.sdp.as_deref().map(str::as_bytes).map(<[u8]>::to_vec).unwrap_or_default(),
            content_type: None,
            extra_headers,
            incoming_source: None,
        };
        let mut resp = generate_response(&txn.request, self.status, &self.reason, &opts);
        // Real UAs may fold multiple echoed Record-Route rows into one comma-
        // separated header (RFC 3261 §7.3.1); reproduce that wire form for UAs the
        // harness picked `Combined` for, so the B2BUA's split-before-§12.1.2-reverse
        // path is exercised on the b-leg route-set capture (see `RecordRouteFold`).
        if txn.agent.rr_fold == RecordRouteFold::Combined {
            fold_record_routes(&mut resp.headers);
        }
        // Responses are routed by Via, not Route (RFC 3261 §18.2.2): send to the
        // request's topmost Via sent-by. With a proxy in the path that Via is
        // the proxy's, so the response correctly traverses it back.
        let dst = top_via_addr(&txn.request).unwrap_or(txn.agent.addr);
        txn.agent.try_send(&SipMessage::Response(resp), dst).await?;
        // §17.1.1.3 UAS obligation (036 ask B): a non-2xx final to an INVITE
        // (initial or re-INVITE) arms the txn-owned ACK wait — the arriving hop
        // ACK is the transaction layer's to claim, in whatever order it lands
        // relative to the body's next receive; `expect_ack` asserts it and the
        // gating `unackedInviteNon2xxFinal` wire rule settles it at finish.
        if (300..700).contains(&self.status) && txn.request.method.as_str() == "INVITE" {
            if let Some(branch) = top_via_branch(&txn.request.headers) {
                txn.agent.acks.arm(txn.request.call_id.clone(), branch);
            }
        }
        Ok(())
    }
}

/// Allow `respond(...).await` directly (no explicit `.send()`), by making the
/// builder awaitable.
impl<'a> std::future::IntoFuture for Respond<'a> {
    type Output = ();
    type IntoFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>>;
    fn into_future(self) -> Self::IntoFuture {
        Box::pin(self.send())
    }
}

// ---------------------------------------------------------------------------
// Proxy / load balancer (loose-routing, Record-Route)
// ---------------------------------------------------------------------------

/// A minimal loose-routing proxy — the test stand-in for the LB front proxy
/// (port target: `sip-front-proxy`). It does the load-bearing routing surgery
/// per RFC 3261 §16:
///   - adds its own **Via** (top) to forwarded requests so responses route back
///     through it (§16.6), and strips that Via from responses (§16.7);
///   - inserts a `;lr` **Record-Route** (top) on dialog-creating INVITEs so both
///     peers route in-dialog requests through it (§16.6.4);
///   - strips its own top **Route** from in-dialog requests it is the loose
///     router for (§16.4) before forwarding.
///
/// It is *stateless* and *scripted*: the test says which way to forward each
/// message (the real proxy resolves the next hop from the top Route / RURI).
#[derive(Clone)]
pub struct Proxy {
    agent: Agent,
}

impl Proxy {
    pub fn addr(&self) -> SocketAddr {
        self.agent.addr
    }
    pub fn name(&self) -> &str {
        &self.agent.name
    }

    fn record_route_value(&self) -> String {
        format!("<sip:{}:{};lr>", self.agent.addr.ip(), self.agent.addr.port())
    }

    /// Receive one request, apply the §16 surgery, and forward it to `next`.
    /// Returns the (rewritten) request for assertions.
    pub async fn forward_request(&self, next: SocketAddr) -> SipRequest {
        let SipMessage::Request(mut req) = self.agent.recv().await else {
            panic!("{} expected a request to forward", self.agent.name);
        };
        // Loose router popping itself off the route set (§16.4) — in-dialog
        // requests (ACK/BYE/…) arrive with our Record-Route as the top Route.
        strip_top_route_if_self(&mut req, self.agent.addr);
        // Record-Route dialog-creating requests so in-dialog traffic returns
        // through us (§16.6.4). A dialog-creating INVITE has no To-tag yet.
        if req.method == "INVITE" && req.to.tag.is_none() {
            prepend_header(&mut req.headers, "Record-Route", &self.record_route_value());
        }
        // Add our Via on top so the response comes back to us (§16.6).
        prepend_header(&mut req.headers, "Via", &self.via_value());
        self.agent.send(&SipMessage::Request(req.clone()), next).await;
        req
    }

    /// Receive one response, strip our Via, and forward it to `next`.
    pub async fn forward_response(&self, next: SocketAddr) -> SipResponse {
        let SipMessage::Response(mut resp) = self.agent.recv().await else {
            panic!("{} expected a response to forward", self.agent.name);
        };
        strip_top_via_if_self(&mut resp.headers, self.agent.addr);
        self.agent.send(&SipMessage::Response(resp.clone()), next).await;
        resp
    }

    fn via_value(&self) -> String {
        format!(
            "SIP/2.0/UDP {}:{};branch={}",
            self.agent.addr.ip(),
            self.agent.addr.port(),
            self.agent.branch()
        )
    }
}

/// Insert a header at the top of the list (RFC 3261 §16.6 prepend semantics for
/// Via / Record-Route).
fn prepend_header(headers: &mut Vec<SipHeader>, name: &str, value: &str) {
    headers.insert(
        0,
        SipHeader {
            name: name.to_string(),
            value: value.to_string(),
        },
    );
}

/// Strip the first Route header if it routes to `me` (the loose router removing
/// itself, §16.4).
fn strip_top_route_if_self(req: &mut SipRequest, me: SocketAddr) {
    if let Some(pos) = req
        .headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("route"))
    {
        let uri = strip_route_uri_to_request_uri(&req.headers[pos].value);
        if uri_to_addr(&uri) == Some(me) {
            req.headers.remove(pos);
        }
    }
}

/// Strip the topmost Via if it is `me`'s (the proxy removing its own Via from a
/// response before forwarding upstream, §16.7).
fn strip_top_via_if_self(headers: &mut Vec<SipHeader>, me: SocketAddr) {
    if let Some(pos) = headers
        .iter()
        .position(|h| h.name.eq_ignore_ascii_case("via"))
    {
        let sent_by = headers[pos]
            .value
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.split(';').next())
            .map(str::trim);
        if let Some(addr) = sent_by.and_then(hostport_to_addr) {
            if addr == me {
                headers.remove(pos);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// RFC 3261 §17.1.1.3 client-transaction auto-ACK context (newkahneed-034
/// ask B): the INVITE (initial or re-INVITE) whose response is being awaited,
/// plus the wire destination it was sent to — the non-2xx ACK belongs to the
/// INVITE transaction (same branch, same CSeq number) and is hop-by-hop, so it
/// follows the SAME path. The surfaces that own an INVITE client transaction
/// ([`ClientInvite`], [`ClientReinvite`], [`InDialogTxn`] for a re-INVITE)
/// thread this into the shared expect helpers so EVERY non-2xx final they
/// surface completes its transaction the way a real txn layer does — the
/// `rfc3261.unackedInviteNon2xxFinal` audit rule gates on it.
struct AckCtx<'a> {
    agent: &'a Agent,
    invite: &'a SipRequest,
    wire_dst: SocketAddr,
}

impl AckCtx<'_> {
    /// ACK `resp` iff it is a non-2xx final belonging to THIS transaction
    /// (CSeq number + method INVITE). Everything else — provisionals, finals
    /// to a CANCEL/BYE on the same socket, a 2xx (whose ACK is a dialog-level
    /// act the scenario performs, §13.2.2.4) — is left alone.
    async fn ack_non_2xx(&self, resp: &SipResponse) -> Result<(), StepError> {
        if resp.status < 300
            || !resp.cseq.method.as_str().eq_ignore_ascii_case("INVITE")
            || resp.cseq.seq != self.invite.cseq.seq
        {
            return Ok(());
        }
        let ack = generate_ack_for_non_2xx(self.invite, resp);
        self.agent.try_send(&SipMessage::Request(ack), self.wire_dst).await
    }
}

/// Build + send the CANCEL for a still-pending (re-)INVITE (RFC 3261 §9.1) —
/// the ONE mechanism behind [`ClientInvite::cancel`], [`ClientReinvite::cancel`]
/// (which unwrap it) and [`CancelHandle::cancel_best_effort`] (which swallows
/// the error — the call is already failing).
async fn try_send_cancel(
    agent: &Agent,
    original_invite: &SipRequest,
    wire_dst: SocketAddr,
) -> Result<(), StepError> {
    let cancel = generate_cancel(&InviteClientTransactionHandle {
        original_invite: original_invite.clone(),
    });
    agent.try_send(&SipMessage::Request(cancel), wire_dst).await
}

/// Panicking veneer over [`try_expect_response`].
async fn expect_response(agent: &Agent, status: u16, ack: Option<&AckCtx<'_>>) -> SipResponse {
    unwrap_step(try_expect_response(agent, status, ack).await)
}

/// Receive the next response WITHOUT asserting its status (an unsolicited `100
/// Trying` is absorbed) — the raw-await the out-of-dialog auth retry
/// ([`OutOfDialogRequest::try_send_authed`]) uses so a `401`/`407` keeps its
/// challenge header. An inbound request where a response is expected is an error.
async fn recv_response_raw(agent: &Agent) -> Result<SipResponse, StepError> {
    loop {
        match agent.try_recv().await? {
            SipMessage::Response(r) if r.status == 100 => continue,
            SipMessage::Response(r) => return Ok(r),
            SipMessage::Request(r) => {
                return Err(StepError::UnexpectedKind {
                    who: agent.name.clone(),
                    detail: format!("got a {} request, expected a response", r.method),
                })
            }
        }
    }
}

/// [`try_expect_response_tolerating`] with an empty tolerate list.
async fn try_expect_response(
    agent: &Agent,
    status: u16,
    ack: Option<&AckCtx<'_>>,
) -> Result<SipResponse, StepError> {
    try_expect_response_tolerating(agent, status, &[], ack).await
}

/// THE client-side expect core, behind every `expect`/`try_expect`
/// (`_tolerating`) on [`ClientInvite`] / [`ClientReinvite`] / [`InDialogTxn`]:
/// wait for the response with status `status`, absorbing an unsolicited `100
/// Trying` (RFC 3261 §8.1.3.2 — a stateful upstream emits it before the first
/// real provisional) and 200-OKing any inbound request whose method is in
/// `tolerate` (e.g. a keepalive OPTIONS racing the awaited response on the
/// same socket). Any non-2xx final belonging to `ack`'s INVITE transaction is
/// auto-ACKed on arrival (§17.1.1.3), matching or not. A wrong status /
/// genuinely unexpected request / timeout is a [`StepError`]; the panicking
/// veneers unwrap it.
async fn try_expect_response_tolerating(
    agent: &Agent,
    status: u16,
    tolerate: &[&str],
    ack: Option<&AckCtx<'_>>,
) -> Result<SipResponse, StepError> {
    loop {
        match agent.try_recv().await? {
            SipMessage::Response(r) if r.status == 100 && status != 100 => continue,
            SipMessage::Response(r) => {
                if let Some(ctx) = ack {
                    ctx.ack_non_2xx(&r).await?;
                }
                if r.status != status {
                    return Err(StepError::WrongStatus {
                        who: agent.name.clone(),
                        expected: status,
                        got: r.status,
                        reason: r.reason.clone(),
                    });
                }
                return Ok(r);
            }
            SipMessage::Request(r) if tolerate.iter().any(|t| r.method == *t) => {
                let mut txn = ServerTxn::from_request(agent.clone(), r);
                txn.respond(200, "OK").try_send().await?;
                continue;
            }
            SipMessage::Request(r) => {
                if agent.ack_obligation_claims(&r) {
                    continue; // txn-owned §17.1.1.3 hop ACK (036 ask B)
                }
                let tolerating = if tolerate.is_empty() {
                    String::new()
                } else {
                    format!(" (tolerating {tolerate:?})")
                };
                return Err(StepError::UnexpectedKind {
                    who: agent.name.clone(),
                    detail: format!(
                        "got a {} request, expected a {status} response{tolerating}",
                        r.method
                    ),
                });
            }
        }
    }
}

/// Unwrap a `<uri>` name-addr / Route value to its bare URI (params after `>`
/// dropped); a bare URI passes through trimmed.
fn unwrap_angle(value: &str) -> String {
    let t = value.trim();
    match (t.find('<'), t.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => t[a + 1..b].to_string(),
        _ => t.to_string(),
    }
}

/// The first Contact URI on a response, unwrapped from `<...>`. Used to learn
/// the dialog remote target.
fn first_contact_uri(resp: &SipResponse) -> Option<String> {
    get_header(&resp.headers, "contact").map(unwrap_angle)
}

/// The `RAck` value acknowledging a reliable provisional (RFC 3262 §7.2):
/// `<RSeq> <CSeq-num> <CSeq-method>`, all read off the 1xx itself. `None` when
/// the response carries no parseable `RSeq` (it is not a reliable provisional).
fn rack_for(reliable_1xx: &SipResponse) -> Option<String> {
    let rseq: u64 = get_header(&reliable_1xx.headers, "rseq")?.trim().parse().ok()?;
    Some(format!("{rseq} {} {}", reliable_1xx.cseq.seq, reliable_1xx.cseq.method))
}

/// Resolve a SIP URI to a socket address (default port 5060, IPv4 fixtures
/// only). Handles `sip:user@host:port`, the userless `sip:host:port;lr` form
/// of a Route/Record-Route URI, and a bare `host:port`.
fn uri_to_addr(uri: &str) -> Option<SocketAddr> {
    let no_scheme = uri
        .strip_prefix("sips:")
        .or_else(|| uri.strip_prefix("sip:"))
        .unwrap_or(uri);
    // Host part is whatever follows the last '@' (none → the whole thing).
    let host_part = no_scheme.rsplit('@').next()?;
    let host_port = host_part.split([';', '?']).next()?.trim();
    hostport_to_addr(host_port)
}

/// Parse a bare `host:port` (or `host`, default port 5060) to a socket address.
fn hostport_to_addr(host_port: &str) -> Option<SocketAddr> {
    if let Ok(sa) = host_port.parse::<SocketAddr>() {
        return Some(sa);
    }
    format!("{host_port}:5060").parse().ok()
}

/// The wire destination for an in-dialog request: the first hop in the route
/// set (the proxy) when present, else the dialog's remote target. For both
/// loose and strict routing the next hop is the address of `route_set[0]`'s
/// URI; with no route set it is the remote target.
fn next_hop(dialog: &StackDialog, fallback: SocketAddr) -> SocketAddr {
    if let Some(top) = dialog.route_set.first() {
        if let Some(addr) = uri_to_addr(&strip_route_uri_to_request_uri(top)) {
            return addr;
        }
    }
    uri_to_addr(&dialog.remote_target).unwrap_or(fallback)
}

/// The address a response to `req` must be sent to: the topmost Via's sent-by
/// (RFC 3261 §18.2.2). (`received=`/`rport=` are not stamped by this harness's
/// `generate_response`, so the sent-by host:port is authoritative here.)
fn top_via_addr(req: &SipRequest) -> Option<SocketAddr> {
    let via = get_header(&req.headers, "via")?;
    // "SIP/2.0/UDP host:port;branch=…" → take the token after the transport,
    // before the first ';'.
    let after_transport = via.split_whitespace().nth(1)?;
    let sent_by = after_transport.split(';').next()?.trim();
    hostport_to_addr(sent_by)
}

#[cfg(test)]
mod auth_seam_tests {
    //! The deferred-by-design [`ChallengeResponder`] retry plumbing (RFC 3261
    //! §22.2), exercised on the fallible INVITE surface. A FAKE responder (a
    //! static credential) proves the ACK→resend→credential→bumped-CSeq path; a
    //! run with NO responder proves the classification is unchanged (a `401`
    //! stays a `WrongStatus`).

    use std::sync::Arc;

    use super::*;
    use crate::realcall::auth::{Challenge, ChallengeResponder};

    const OFFER: &str = "v=0\r\no=a 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

    /// A static-credential responder: returns a fixed `Authorization` value for
    /// any challenge (the deferred seam's simplest possible implementation — real
    /// digest would hash `challenge.header_value` + `method`/`ruri`). Records what
    /// it was asked so the test can assert the request-line inputs reached it.
    struct FakeResponder {
        credential: String,
        seen: std::sync::Mutex<Vec<(u16, String, String)>>,
    }
    impl ChallengeResponder for FakeResponder {
        fn respond(&self, challenge: &Challenge, method: &str, ruri: &str) -> Option<String> {
            self.seen.lock().unwrap().push((
                challenge.status,
                method.to_string(),
                ruri.to_string(),
            ));
            Some(self.credential.clone())
        }
    }

    /// Direct plumbing: alice INVITEs a UAS that `401`s once (with a
    /// `WWW-Authenticate` challenge) then admits. The retry ACKs the challenge,
    /// adds the responder's `Authorization`, bumps the CSeq, resends, and the call
    /// completes — proving [`ClientInvite::ack_and_resend_with_auth`] end to end.
    #[tokio::test(start_paused = true)]
    async fn auth_retry_acks_resends_with_credential_and_bumped_cseq() {
        let h = Harness::new("auth-retry-plumbing");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let responder = FakeResponder {
            credential: "Digest username=\"alice\", realm=\"sip\", nonce=\"abc\", response=\"deadbeef\""
                .to_string(),
            seen: std::sync::Mutex::new(Vec::new()),
        };

        // Alice's INVITE #1 goes straight to the server.
        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;

        // The server challenges with a 401 + WWW-Authenticate.
        let mut chal = server.try_receive("INVITE").await.unwrap();
        assert_eq!(chal.request().cseq.seq, 1, "first INVITE is CSeq 1");
        chal.respond(401, "Unauthorized")
            .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"abc\"")
            .try_send()
            .await
            .unwrap();

        // Alice sees the 401 (raw, un-asserted) and drives the retry.
        let resp = call.try_recv_response().await.unwrap();
        assert_eq!(resp.status, 401);
        let resent = call.ack_and_resend_with_auth(&resp, &responder).await.unwrap();
        assert!(resent, "responder returned a credential → a resend happened");

        // The responder saw the challenge status + the request-line inputs.
        {
            let seen = responder.seen.lock().unwrap();
            assert_eq!(seen.len(), 1);
            assert_eq!(seen[0].0, 401);
            assert_eq!(seen[0].1, "INVITE");
            assert!(seen[0].2.starts_with("sip:server@"), "ruri passed through: {}", seen[0].2);
        }

        // The server first sees the ACK for the 401 (RFC 3261 §17.1.1.3)…
        let ack = server.try_receive("ACK").await.unwrap();
        assert_eq!(ack.request().cseq.seq, 1, "the non-2xx ACK reuses the INVITE CSeq");

        // …then the resent, authenticated INVITE #2: CSeq bumped, Authorization added.
        let mut admit = server.try_receive("INVITE").await.unwrap();
        assert_eq!(admit.request().cseq.seq, 2, "the retried INVITE bumps the CSeq (§22.2)");
        assert!(
            get_header(&admit.request().headers, "authorization")
                .is_some_and(|v| v.starts_with("Digest ")),
            "the retried INVITE carries the responder's Authorization",
        );

        // The server admits; alice completes the call.
        admit.respond(180, "Ringing").try_send().await.unwrap();
        call.try_expect(180).await.unwrap();
        admit.respond(200, "OK").with_sdp(OFFER).try_send().await.unwrap();
        call.try_expect(200).await.unwrap();
        let mut dialog = call.ack().await;
        server.try_receive("ACK").await.unwrap();

        // Teardown.
        let mut bye = dialog.bye().await;
        server.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
        bye.try_expect(200).await.unwrap();

        let _ = h.finish().await;
    }

    /// newkahneed-033 ask D: `try_expect_final` absorbs (and collects) any
    /// interleaved provisionals — the SIPp-`optional` semantics — instead of
    /// erroring on a 1xx the body did not hard-code, and still learns the
    /// dialog state the final confirms (the ACK/BYE route correctly after).
    #[tokio::test(start_paused = true)]
    async fn try_expect_final_absorbs_and_collects_provisionals() {
        let h = Harness::new("try-expect-final");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.try_receive("INVITE").await.unwrap();
        // A relay-timing-dependent provisional mix: 180 then 183, then the 200.
        uas.respond(180, "Ringing").try_send().await.unwrap();
        uas.respond(183, "Session Progress").try_send().await.unwrap();
        uas.respond(200, "OK").with_sdp(OFFER).try_send().await.unwrap();

        let (answer, provisionals) = call.try_expect_final(200).await.unwrap();
        assert_eq!(answer.status, 200);
        assert_eq!(
            provisionals.iter().map(|p| p.status).collect::<Vec<_>>(),
            vec![180, 183],
            "every absorbed 1xx is collected, in arrival order"
        );

        // The learned dialog state routes the ACK + teardown correctly.
        let mut dialog = call.ack().await;
        server.try_receive("ACK").await.unwrap();
        let mut bye = dialog.bye().await;
        server.try_receive("BYE").await.unwrap().respond(200, "OK").try_send().await.unwrap();
        bye.try_expect(200).await.unwrap();
        let _ = h.finish().await;
    }

    /// newkahneed-033 ask C: `try_receive_tolerating_blocking` waits for the
    /// sentinel method, 200-OKs the tolerated traffic in between, and RETURNS
    /// the absorbed requests so the body can assert them — where `quiesce`
    /// blind-drains and a lost sentinel becomes silent success (here it is a
    /// `Timeout` error, asserted first on an idle socket).
    #[tokio::test(start_paused = true)]
    async fn try_receive_tolerating_blocking_collects_absorbed_and_times_out() {
        let h = Harness::new("blocking-tolerant-receive");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        // Establish A↔server.
        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.try_receive("INVITE").await.unwrap();
        uas.respond(200, "OK").with_sdp(OFFER).try_send().await.unwrap();
        call.try_expect(200).await.unwrap();
        let mut dialog = call.ack().await;
        server.try_receive("ACK").await.unwrap();

        // A lost sentinel is a DETECTABLE failure: nothing is in flight, so the
        // blocking receive times out instead of silently succeeding.
        match server.try_receive_tolerating_blocking("BYE", &["NOTIFY"]).await {
            Err(StepError::Timeout { .. }) => {}
            Err(e) => panic!("an absent sentinel must surface as Timeout, got {e}"),
            Ok((txn, _)) => panic!(
                "an absent sentinel must surface as Timeout, got a {} request",
                txn.request().method
            ),
        }

        // NOTIFY(s) then the BYE — the nondeterministic-count release pattern
        // (the ct_refer shape). The primitive 200s the NOTIFY, returns on the
        // BYE, and hands the absorbed NOTIFY back for assertion.
        let mut notify =
            dialog.send_request(InDialogMethod::Notify).try_send().await.unwrap();
        let mut bye = dialog.bye().await;

        let (mut bye_txn, absorbed) =
            server.try_receive_tolerating_blocking("BYE", &["NOTIFY"]).await.unwrap();
        assert_eq!(
            absorbed.iter().map(|r| r.method.to_string()).collect::<Vec<_>>(),
            vec!["NOTIFY".to_string()],
            "the absorbed traffic is returned, assertable"
        );
        bye_txn.respond(200, "OK").try_send().await.unwrap();

        // Alice's side settles: the primitive's 200 (NOTIFY) and her BYE 200.
        notify.try_expect(200).await.unwrap();
        bye.try_expect(200).await.unwrap();
        let _ = h.finish().await;
    }

    /// The out-of-dialog twin ([`OutOfDialogRequest::try_send_authed`], the future
    /// REGISTER seam): a server `401`s the first OPTIONS then `200`s the
    /// credentialed resend. No ACK (a non-INVITE final needs none, §17.1.2.2); the
    /// resend bumps the CSeq and carries the responder's `Authorization`.
    #[tokio::test(start_paused = true)]
    async fn out_of_dialog_try_send_authed_retries_once() {
        let h = Harness::new("auth-ood-retry");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let responder: Arc<dyn ChallengeResponder> = Arc::new(FakeResponder {
            credential: "Digest username=\"alice\", realm=\"sip\", response=\"y\"".to_string(),
            seen: std::sync::Mutex::new(Vec::new()),
        });

        let server_rx = server.clone();
        let srv = tokio::spawn(async move {
            let server = server_rx;
            // First OPTIONS → 401.
            let mut c = server.try_receive("OPTIONS").await.unwrap();
            assert_eq!(c.request().cseq.seq, 1);
            assert!(get_header(&c.request().headers, "authorization").is_none());
            c.respond(401, "Unauthorized")
                .with_header("WWW-Authenticate", "Digest realm=\"sip\", nonce=\"n\"")
                .try_send()
                .await
                .unwrap();
            // Credentialed resend → 200. CSeq bumped, Authorization present.
            let mut c2 = server.try_receive("OPTIONS").await.unwrap();
            assert_eq!(c2.request().cseq.seq, 2, "the authed resend bumps the CSeq");
            assert!(
                get_header(&c2.request().headers, "authorization").is_some(),
                "the resend carries the Authorization",
            );
            c2.respond(200, "OK").try_send().await.unwrap();
        });

        let resp = alice
            .request(OutOfDialogMethod::Options, &server)
            .try_send_authed(Some(responder.as_ref()), 200)
            .await
            .expect("the authenticated OPTIONS resolves to 200");
        assert_eq!(resp.status, 200);

        srv.await.unwrap();
        let _ = h.finish().await;
    }

    /// The out-of-dialog path with NO responder: the `401` surfaces as a plain
    /// `WrongStatus` (no retry), unchanged from `try_send` + `try_expect`.
    #[tokio::test(start_paused = true)]
    async fn out_of_dialog_without_responder_surfaces_401() {
        let h = Harness::new("auth-ood-no-responder");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let server_rx = server.clone();
        let srv = tokio::spawn(async move {
            let mut c = server_rx.try_receive("OPTIONS").await.unwrap();
            c.respond(401, "Unauthorized").try_send().await.unwrap();
        });

        match alice
            .request(OutOfDialogMethod::Options, &server)
            .try_send_authed(None, 200)
            .await
        {
            Err(StepError::WrongStatus { got: 401, expected: 200, .. }) => {}
            Err(other) => panic!("expected WrongStatus 200/401, got {other:?}"),
            Ok(r) => panic!("expected a 401 deviation, got {}", r.status),
        }

        srv.await.unwrap();
        let _ = h.finish().await;
    }
}

#[cfg(test)]
mod txn_view_tests {
    //! newkahneed-034: the §17.2 once-and-only-once receive view ([`TxnView`]).
    //! Pure verdict tests pin the keying/byte-identity semantics; the
    //! paused-clock integration tests pin the end-to-end contract (a Timer-A
    //! style duplicate never surfaces, `receive_absorbing` lists unneeded;
    //! `wire_view()` restores the raw surface).

    use super::*;

    const OFFER: &str = "v=0\r\no=a 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

    fn parse(raw: &str) -> SipMessage {
        CustomParser::new().parse(raw.as_bytes()).expect("test fixture parses")
    }

    const INV: &str = "INVITE sip:bob@10.0.0.2 SIP/2.0\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-tv-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>\r\n\
        Call-ID: tv-c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    fn verdict_of(view: &TxnView, raw: &str) -> TxnVerdict {
        view.verdict(raw.as_bytes(), &parse(raw))
    }

    #[test]
    fn byte_identical_request_rearrival_is_absorbed() {
        let view = TxnView::functional();
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Surface));
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Absorb));
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Absorb));
    }

    #[test]
    fn same_key_different_bytes_surfaces_not_absorbed() {
        let view = TxnView::functional();
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Surface));
        // Same key, different bytes (an extra header): NOT a retransmission, so
        // it is delivered — the receive view only ever absorbs a provable
        // byte-identical duplicate, never masks a differing datagram.
        let mutated = INV.replace("Call-ID: tv-c1", "Call-ID: tv-c1\r\nX-Mutant: yes");
        assert_eq!(verdict_of(&view, &mutated), TxnVerdict::Surface);
        // And that delivered datagram's OWN retransmit now dedups.
        assert_eq!(verdict_of(&view, &mutated), TxnVerdict::Absorb);
    }

    #[test]
    fn same_branch_different_call_id_both_surface() {
        // A branch reused across TWO calls (a deterministic harness-proxy
        // artifact) is NOT one transaction — the Call-ID disambiguates, so
        // both surface and neither is mistaken for the other's retransmission.
        let view = TxnView::functional();
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Surface));
        let other_call = INV.replace("Call-ID: tv-c1", "Call-ID: tv-c2");
        assert!(matches!(verdict_of(&view, &other_call), TxnVerdict::Surface));
    }

    #[test]
    fn distinct_branch_and_shared_branch_cancel_both_surface() {
        let view = TxnView::functional();
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Surface));
        // A NEW transaction (fresh branch) of the same method surfaces.
        let second = INV.replace("z9hG4bK-tv-1", "z9hG4bK-tv-2");
        assert!(matches!(verdict_of(&view, &second), TxnVerdict::Surface));
        // A CANCEL sharing the INVITE's branch is its OWN server transaction
        // (§17.2.3 keys on method too) — surfaced, not absorbed.
        // (A pre-RFC3261 cookie-less branch never reaches the verdict: the
        // parser rejects it — the `z9hG4bK` guard in [`TxnView::verdict`] is
        // pure defense in depth.)
        let cancel = INV
            .replace("INVITE sip:bob@10.0.0.2 SIP/2.0", "CANCEL sip:bob@10.0.0.2 SIP/2.0")
            .replace("CSeq: 1 INVITE", "CSeq: 1 CANCEL");
        assert!(matches!(verdict_of(&view, &cancel), TxnVerdict::Surface));
    }

    const FINAL_200: &str = "SIP/2.0 200 OK\r\n\
        Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-tv-1\r\n\
        From: <sip:alice@10.0.0.1>;tag=a1\r\n\
        To: <sip:bob@10.0.0.2>;tag=b1\r\n\
        Call-ID: tv-c1\r\n\
        CSeq: 1 INVITE\r\n\
        Content-Length: 0\r\n\r\n";

    #[test]
    fn final_repeat_absorbed_but_forked_2xx_surfaces() {
        let view = TxnView::functional();
        assert!(matches!(verdict_of(&view, FINAL_200), TxnVerdict::Surface));
        // Timer-G style byte-identical repeat: absorbed.
        assert!(matches!(verdict_of(&view, FINAL_200), TxnVerdict::Absorb));
        // A forked 2xx — same key, DIFFERENT To-tag/bytes — is a real signal.
        let fork = FINAL_200.replace(";tag=b1", ";tag=b2");
        assert!(matches!(verdict_of(&view, &fork), TxnVerdict::Surface));
    }

    #[test]
    fn provisionals_are_never_deduped() {
        let view = TxnView::functional();
        // A byte-identical second 180 is the ring-again observable (033-D2):
        // the functional lane is exactly where it must stay visible.
        let ringing = FINAL_200.replace("SIP/2.0 200 OK", "SIP/2.0 180 Ringing");
        assert!(matches!(verdict_of(&view, &ringing), TxnVerdict::Surface));
        assert!(matches!(verdict_of(&view, &ringing), TxnVerdict::Surface));
    }

    #[test]
    fn wire_view_surfaces_everything() {
        let view = TxnView::wire();
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Surface));
        assert!(matches!(verdict_of(&view, INV), TxnVerdict::Surface));
        assert!(matches!(verdict_of(&view, FINAL_200), TxnVerdict::Surface));
        assert!(matches!(verdict_of(&view, FINAL_200), TxnVerdict::Surface));
    }

    /// The headline contract: a Timer-A style INVITE retransmission never
    /// surfaces, so the callee needs NO `receive_absorbing` list — the exact
    /// pattern that used to require one (silent-callee duplicates queued ahead
    /// of the ACK; pre-034 the `receive("ACK")` below panicked with
    /// "expected a ACK request, got INVITE").
    #[tokio::test(start_paused = true)]
    async fn invite_retransmits_absorbed_without_lists() {
        let h = Harness::new("txn-view-invite-retransmit");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        // Byte-identical Timer-A duplicates, straight from the original request.
        let dup = SipMessage::Request(call.original_invite.clone());
        alice.send(&dup, call.wire_dst).await;
        alice.send(&dup, call.wire_dst).await;

        let mut uas = server.receive("INVITE").await;
        uas.respond(180, "Ringing").send().await;
        call.expect(180).await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        // The two duplicates are queued ahead of the ACK — absorbed below the API.
        server.receive("ACK").await;

        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        bye.expect(200).await;
        let _ = h.finish().await;
    }

    /// A byte-identical 2xx repeat (Timer-G style) is absorbed — and can no
    /// longer be mis-taken for the answer to a LATER transaction (the old
    /// status-only `expect(200)` would have returned the duplicate 200-INVITE
    /// as the BYE's answer).
    #[tokio::test(start_paused = true)]
    async fn duplicate_final_not_mistaken_for_later_answer() {
        let h = Harness::new("txn-view-final-dedup");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        let mut uas = server.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        // The 2xx repeat: sticky To-tag + same SDP ⇒ byte-identical.
        uas.respond(200, "OK").with_sdp(OFFER).send().await;

        let mut dialog = call.ack().await;
        server.receive("ACK").await;
        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        let resp = bye.expect(200).await;
        assert_eq!(
            resp.cseq.method.to_string(),
            "BYE",
            "the duplicate 200-INVITE was absorbed, not returned as the BYE answer"
        );
        let _ = h.finish().await;
    }

    /// `wire_view()` restores the raw surface: the duplicate SURFACES again and
    /// the historical `receive_absorbing` idiom is once more the caller's job —
    /// the sanctioned escape hatch for tests whose subject is retransmission.
    #[tokio::test(start_paused = true)]
    async fn wire_view_restores_raw_duplicates() {
        let h = Harness::new("txn-view-wire-optout");
        let alice = h.agent("alice", "127.0.0.1:5060").await;
        let server = h.agent("server", "127.0.0.1:5070").await;
        server.wire_view();

        let mut call = alice.invite(&server).with_sdp(OFFER).send().await;
        alice.send(&SipMessage::Request(call.original_invite.clone()), call.wire_dst).await;

        let mut uas = server.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(OFFER).send().await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        // The duplicate INVITE is still queued and SURFACES — absorb it the old way.
        server.receive_absorbing("ACK", &["INVITE"]).await;

        let mut bye = dialog.bye().await;
        server.receive("BYE").await.respond(200, "OK").send().await;
        bye.expect(200).await;
        let _ = h.finish().await;
    }
}
