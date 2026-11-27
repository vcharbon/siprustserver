//! **The identity of a call a stuck poll held, refused by the survivor.** A
//! routing answer sticks the turn of every call it reaches (ADR-0057 §6): the
//! primary's consult never returns, the host reads the poll stuck, discharges
//! the call (its backup's copy deleted) and exits. The caller's previous hop
//! gives up on the INVITE and retries it on the same Call-ID and From tag,
//! the next CSeq (RFC 3261 §8.1.3.5). With the primary gone the proxy places
//! the retry on the survivor, where the call is unknown: a consult there meets
//! the same answer and sticks the survivor too. A hop that heard the 100
//! retries on its Timer C (more than 3 min, RFC 3261 §16.6 item 11), its
//! copies on Timer B (64·T1): for Timer C's 3 min plus 64·T1 (212 s) after it
//! applied the discharge's delete, the survivor answers an initial INVITE on
//! that identity the bare 503 before any consult, emergency included, and
//! serves every other call.
//!
//! ```text
//!   0 s      alice ──INVITE──▶ proxy ──▶ b1: created, copy on b2, 100 (ADR-0070),
//!            the consult never returns
//!   1 s      b1's stuck poll read: the call discharged (b2's copy deleted),
//!            b1 exits and departs
//!   2 s      alice CANCELs the INVITE (RFC 3261 §9.1): 481 from b2, no final
//!   2.5 s    alice ──INVITE (next CSeq)──▶ proxy ──▶ b2: 503, no consult
//!   2.6 s    another identity: consulted, answered, hung up; then
//!            alice ──emergency INVITE (next CSeq)──▶ b2: 503, no consult
//!   35 s     2 s past 64·T1 from the delete: 503, no consult
//!   211 s    2 s before 212 s from the delete: 503, no consult
//!   215 s    2 s past 212 s from the delete: the identity is consulted and served
//! ```
//!
//! The timing axes: the consult sticks at the INVITE's turn; the stuck read
//! falls inside the setup budget (5 s), which would end the consult with a
//! 503; the retries fall 1.5 s after the delete, 2 s past 64·T1 from it and
//! 2 s before 212 s from it; the last INVITE 2 s past 212 s.
//!
//! When b1's 100 is lost on its way to alice, her Timer A copies go on: the
//! one at 0.5 s reaches b1, the one at 1.5 s reaches b2 under the cookie
//! naming b1, another primary's INVITE. b2 answers that copy the bare 503
//! too: the caller ends her transaction on a final (ADR-0022), and nothing is
//! consulted.
//!
//! Beyond the failover sweep: a query of death costs one worker, not both.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::admission::Class;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, NewCallRequest, NewCallResponse, ScriptedDecisionEngine,
};
use b2bua::limiter::NoopLimiter;
use b2bua::new_calls::Refusal;
use b2bua::StuckDischarge;
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, FailoverHarness, PartitionRole, ProxySut,
    ReplicatedB2buaSut,
};
use scenario_harness::callflow::{hangup, ANSWER_SDP, OFFER_SDP};
use scenario_harness::{Agent, ClientInvite, Party};
use sip_message::header::HeaderName;
use sip_net::{Arrival, PreIngressAction, PreIngressHook};
use tokio::time::Instant;

use crate::departure_support::{call_id_placed_on, spawn_front_proxy, B1, B2};

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
/// The From tag of the identity whose routing answer sticks a turn.
const STUCK_FROM_TAG: &str = "stuck-identity-from-tag";
/// The From tag of another call.
const OTHER_FROM_TAG: &str = "another-identity-from-tag";

/// RFC 3261 T1.
const T1: Duration = Duration::from_millis(500);
/// How long the survivor refuses the identity after the delete: a previous
/// hop's Timer C (3 min at least, RFC 3261 §16.6 item 11) plus 64·T1.
const WINDOW: Duration = Duration::from_secs(212);
/// The `Resource-Priority` value marking an INVITE emergency (RFC 4412).
const EMERGENCY_PRIORITY: &str = "esnet.0";
/// How long after the INVITE the host reads the poll stuck and discharges its
/// call: under the setup budget (5 s), which would end the consult.
const READ_STUCK: Duration = Duration::from_secs(1);
/// The bound the host gives the discharge (its release flush bound).
const DISCHARGE_WITHIN: Duration = Duration::from_secs(1);
/// When the previous hop gives up on the INVITE, after the discharge.
const GIVE_UP: Duration = Duration::from_secs(2);
/// When the retry is sent: inside 64·T1 of the delete.
const RETRY: Duration = Duration::from_millis(2_500);
/// Past the simulated transit of a request and its answer.
const TRANSIT: Duration = Duration::from_millis(100);
/// How far past 64·T1 from the delete a retry on the previous hop's Timer C
/// is sent.
const PAST_TIMER_B: Duration = Duration::from_secs(2);
/// How long before the window ends the last refused retry is sent.
const BEFORE_THE_WINDOW_ENDS: Duration = Duration::from_secs(2);
/// How far past the window the last INVITE is sent.
const PAST_WINDOW: Duration = Duration::from_secs(2);
/// When alice's Timer A copies of an INVITE that drew no response are sent
/// (RFC 3261 §17.1.1.2): T1, then 3·T1 after it.
const FIRST_COPY: Duration = T1;
const SECOND_COPY: Duration = Duration::from_millis(1_500);

/// What alice's bind lets through.
#[derive(Clone, Copy)]
enum AliceHears {
    /// Every datagram.
    Everything,
    /// Every datagram but a 100 Trying: lost on the way.
    NoTrying,
}

impl AliceHears {
    /// The bind hook dropping what alice does not hear.
    fn hook(self) -> Option<PreIngressHook> {
        match self {
            AliceHears::Everything => None,
            AliceHears::NoTrying => Some(Arc::new(|arrival: &mut Arrival, _src, _depth| {
                if arrival.raw().starts_with(b"SIP/2.0 100 ") {
                    PreIngressAction::Drop
                } else {
                    PreIngressAction::Accept
                }
            })),
        }
    }
}

/// A routing backend whose answer for one Call-ID sticks the turn that asked
/// it, while armed: the consult never returns. Every other call is routed to
/// the callee. Each worker has its own, recording the Call-IDs it was asked
/// and the task the stuck consult runs on.
struct StickyRouting {
    route: Arc<dyn CallDecisionEngine>,
    sticky: String,
    armed: Arc<AtomicBool>,
    seen: Arc<Seen>,
}

/// What one worker's routing backend saw.
#[derive(Default)]
struct Seen {
    consults: Mutex<Vec<String>>,
    stuck_task: Mutex<Option<tokio::task::Id>>,
}

impl Seen {
    /// How many consults asked about `call_id`.
    fn consults_of(&self, call_id: &str) -> usize {
        self.consults.lock().unwrap().iter().filter(|c| *c == call_id).count()
    }

    /// The task of the consult that stuck, once one did.
    fn stuck_task(&self) -> Option<tokio::task::Id> {
        *self.stuck_task.lock().unwrap()
    }
}

#[async_trait]
impl CallDecisionEngine for StickyRouting {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        self.seen.consults.lock().unwrap().push(req.call_id.clone());
        if self.armed.load(Ordering::SeqCst) && req.call_id == self.sticky {
            *self.seen.stuck_task.lock().unwrap() = tokio::task::try_id();
            std::future::pending::<()>().await;
        }
        self.route.new_call(req).await
    }
    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        self.route.call_failure(req).await
    }
    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        self.route.call_refer(req).await
    }
}

/// A routing backend sticking `sticky` while `armed`, and what it sees.
fn sticky_routing(
    sticky: &str,
    armed: &Arc<AtomicBool>,
) -> (Arc<dyn CallDecisionEngine>, Arc<Seen>) {
    let seen = Arc::new(Seen::default());
    let engine = Arc::new(StickyRouting {
        route: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
        sticky: sticky.to_string(),
        armed: armed.clone(),
        seen: seen.clone(),
    });
    (engine, seen)
}

/// One worker of the pair and what its routing backend sees.
struct Worker {
    sut: ReplicatedB2buaSut,
    seen: Arc<Seen>,
}

/// The cluster, the parties and the two identities.
struct Scene {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    proxy: ProxySut,
    b1: Worker,
    b2: Worker,
    /// Whether the stuck identity's routing answer sticks a turn.
    armed: Arc<AtomicBool>,
    /// The identity whose routing answer sticks a turn, placed on b1.
    stuck: (String, &'static str),
    /// Another call's identity.
    other: (String, &'static str),
}

impl Scene {
    /// Both workers behind the proxy, each on its own routing backend
    /// sticking the stuck identity's Call-ID, ready at steady state; alice
    /// hears what `hears` lets through.
    async fn start(name: &str, hears: AliceHears) -> Scene {
        let mut fh = FailoverHarness::new(name, &["b1", "b2"]);
        let alice = match hears.hook() {
            None => fh.agent("alice", ALICE).await,
            Some(hook) => fh.agent_with_pre_ingress("alice", ALICE, hook).await,
        };
        let bob = fh.agent("bob", BOB).await;
        let proxy = spawn_front_proxy(&fh).await;
        let stuck = (call_id_placed_on("b1", "stuck-identity"), STUCK_FROM_TAG);
        let other = (call_id_placed_on("b2", "another-identity"), OTHER_FROM_TAG);
        let armed = Arc::new(AtomicBool::new(true));
        let (callee, outbound) = (("127.0.0.1", 5070), ("127.0.0.1", 5080));
        let (d1, seen1) = sticky_routing(&stuck.0, &armed);
        let (d2, seen2) = sticky_routing(&stuck.0, &armed);
        let noop = || Arc::new(NoopLimiter);
        let b1 =
            fh.spawn_worker_limited("b1", "b1", B1, &["b2"], callee, outbound, d1, noop()).await;
        let b2 =
            fh.spawn_worker_limited("b2", "b2", B2, &["b1"], callee, outbound, d2, noop()).await;
        fh.advance(Duration::from_millis(500)).await;
        assert!(b1.is_ready() && b2.is_ready(), "both workers ready at steady state");
        let (b1, b2) = (Worker { sut: b1, seen: seen1 }, Worker { sut: b2, seen: seen2 });
        Scene { fh, alice, bob, proxy, b1, b2, armed, stuck, other }
    }

    /// The stuck identity, borrowed.
    fn stuck(&self) -> (&str, &str) {
        (&self.stuck.0, self.stuck.1)
    }

    /// The other identity, borrowed.
    fn other(&self) -> (&str, &str) {
        (&self.other.0, self.other.1)
    }

    /// alice's INVITE under `identity` (Call-ID, From tag) with CSeq `cseq`,
    /// through the proxy.
    async fn invite(&self, identity: (&str, &str), cseq: u32) -> ClientInvite {
        self.invite_as(identity, cseq, Class::Normal).await
    }

    /// alice's INVITE under `identity` with CSeq `cseq` in `class`: an
    /// emergency one carries an emergency `Resource-Priority`.
    async fn invite_as(&self, identity: (&str, &str), cseq: u32, class: Class) -> ClientInvite {
        let proxy: SocketAddr = self.proxy.addr();
        let invite = self.alice.invite(&self.bob).identity(identity.0, identity.1).cseq(cseq);
        let invite = match class {
            Class::Emergency => invite.with_header("Resource-Priority", EMERGENCY_PRIORITY),
            _ => invite,
        };
        invite.with_sdp(OFFER_SDP).through(proxy).send().await
    }

    /// bob answers `call`, bob hangs up: the call is served.
    async fn answer_and_hang_up(&self, mut call: ClientInvite) {
        let mut uas = self.bob.receive("INVITE").await;
        uas.respond(200, "OK").with_sdp(ANSWER_SDP).await;
        call.expect(200).await;
        let mut dialog = call.ack().await;
        self.bob.receive("ACK").await;
        hangup(&mut dialog, &self.bob).await;
    }

    /// Step 1: alice's INVITE on the stuck identity sticks b1's consult; the
    /// host discharges the call, b1 exits and departs; alice gives up on the
    /// INVITE. The instant b2 applied the discharge's delete.
    async fn stick_and_discharge(&mut self) -> Instant {
        let first = self.invite(self.stuck(), Party::Caller.first_cseq()).await;
        self.fh.advance(READ_STUCK).await;
        let delete_applied = self.discharge().await;
        self.fh.advance(GIVE_UP - READ_STUCK).await;
        first.cancel().await.expect(481).await;
        delete_applied
    }

    /// The host reads b1's consult of the stuck identity stuck and
    /// discharges its call; b2 applies the delete; b1 exits and departs. The
    /// instant b2 applied the delete.
    async fn discharge(&mut self) -> Instant {
        let task = self.b1.seen.stuck_task().expect("b1's consult of the call sticks");
        let discharged = call_ref("b1", self.stuck());
        let copy = self.b2.sut.get(PartitionRole::Backup, "b1", &discharged).await;
        assert!(copy.is_some(), "b2 holds the copy of the call b1's consult sticks");
        let discharge = self.b1.sut.discharge_stuck(&discharged, task, DISCHARGE_WITHIN).await;
        assert_eq!(discharge, Some(StuckDischarge::BackupApplied), "the discharge ended");
        assert!(!self.b2.sut.holds_any_trace(&discharged).await, "b2 applied the delete");
        let delete_applied = Instant::now();
        self.fh.mark("b1", None, "crash", "the stuck exit");
        self.b1.sut.crash();
        self.fh.depart("b1");
        delete_applied
    }

    /// Step 2: alice's INVITE in `class` on the stuck identity with CSeq
    /// `cseq`, inside the window: b2 answers it the bare 503 at once, never
    /// asks its routing, and dials no callee.
    async fn refused_inside_the_window(&self, cseq: u32, class: Class) {
        let dialled = self.invites_to_bob();
        let refused = self.refused_on_b2(class);
        let mut retry = self.invite_as(self.stuck(), cseq, class).await;
        self.fh.advance(TRANSIT).await;
        let consults = self.b2.seen.consults_of(self.stuck().0);
        assert_eq!(consults, 0, "b2 never asks the identity's routing");
        assert!(self.b2.seen.stuck_task().is_none(), "b2 does not stick");
        assert_bare(&retry.expect(503).await);
        assert_eq!(self.invites_to_bob(), dialled, "no callee dialled");
        assert_eq!(self.refused_on_b2(class), refused + 1, "counted as a discharged identity");
    }

    /// The new calls in `class` b2 refused on a discharged identity.
    fn refused_on_b2(&self, class: Class) -> u64 {
        self.b2.sut.metrics().new_calls().rejected(Refusal::Discharged, class)
    }

    /// The INVITEs bob received so far.
    fn invites_to_bob(&self) -> usize {
        let to_bob = |e: &&sip_net_testkit::RecordedSipEntry| e.to == self.bob.addr();
        self.fh
            .sip_entries()
            .iter()
            .filter(to_bob)
            .filter(|e| e.raw.starts_with(b"INVITE "))
            .count()
    }

    /// Step 3: a call on the other identity, inside the window, is consulted
    /// and served.
    async fn serve_the_other_identity(&self) {
        let call = self.invite(self.other(), Party::Caller.first_cseq()).await;
        self.answer_and_hang_up(call).await;
        assert_eq!(self.b2.seen.consults_of(self.other().0), 1, "the other identity is consulted");
    }

    /// Step 4: past the window from `delete_applied`, with the routing answer
    /// no longer sticking, an INVITE on the stuck identity is consulted and
    /// served.
    async fn serve_the_identity_past_the_window(&self, delete_applied: Instant, cseq: u32) {
        let past = delete_applied + WINDOW + PAST_WINDOW;
        self.fh.advance(past - Instant::now()).await;
        self.armed.store(false, Ordering::SeqCst);
        let consulted = self.b2.seen.consults_of(self.stuck().0);
        let call = self.invite(self.stuck(), cseq).await;
        self.answer_and_hang_up(call).await;
        assert_eq!(
            self.b2.seen.consults_of(self.stuck().0),
            consulted + 1,
            "past the window the identity is consulted"
        );
    }

    /// Every call over on b2: the discharged one and those of `served`,
    /// served on b2, each with its one CDR, the latter fully released; a
    /// refused INVITE wrote none. The RFC audit clean.
    async fn finish(self, name: &str, served: &[(&str, &str)]) {
        let discharged = call_ref("b1", self.stuck());
        let served: Vec<String> = served.iter().map(|id| call_ref("b2", *id)).collect();
        let drained = self
            .fh
            .settle_terminal(async || {
                let mut clean = self.b2.sut.memory_clean();
                for call_ref in std::iter::once(&discharged).chain(&served) {
                    clean &= !self.b2.sut.holds_any_trace(call_ref).await;
                }
                clean
            })
            .await;
        self.fh.linger_peers(&[&self.alice, &self.bob], Duration::from_secs(3)).await;
        assert!(drained, "b2 holds nothing of any call");
        let nodes = [&self.b1.sut, &self.b2.sut];
        assert_eq!(total_cdrs_for(&nodes, &discharged), 1, "the discharge's CDR");
        for call_ref in &served {
            assert_eq!(total_cdrs_for(&nodes, call_ref), 1, "{call_ref}: its one CDR");
            assert_call_fully_released(&[&self.b2.sut], call_ref).await;
        }
        self.fh.assert_sip_rfc_clean(name);
    }
}

/// `resp` is the bare 503: no `Retry-After`, which would keep the previous
/// hop off the healthy survivor (RFC 3261 §21.5.4), and no `Reason`.
fn assert_bare(resp: &sip_message::SipResponse) {
    assert_eq!(resp.reason(), "Service Unavailable");
    assert!(!resp.has(&HeaderName::RetryAfter), "no Retry-After");
    assert!(!resp.has(&HeaderName::Reason), "no Reason");
}

/// The `callRef` of the call `identity` (Call-ID, From tag) opens on `primary`.
fn call_ref(primary: &str, identity: (&str, &str)) -> String {
    call::derive_call_ref(primary, identity.0, identity.1)
}

/// The retries on the stuck identity reach b2 inside the window, 1.5 s after
/// the delete, an emergency one next, then 2 s past 64·T1 from the delete (a
/// retry on the previous hop's Timer C) and 2 s before the window ends: b2
/// answers each the bare 503 at once and never asks its routing, so b2 does
/// not stick; another identity is served meanwhile, and 2 s past the window
/// the identity is served again, the window not stretched by the refusals.
#[tokio::test(start_paused = true)]
async fn the_survivor_refuses_the_identity_of_a_call_a_stuck_poll_held_through_timer_c() {
    let name = "discharged-identity-refused";
    let mut s = Scene::start(name, AliceHears::Everything).await;
    let first_cseq = Party::Caller.first_cseq();
    let delete_applied = s.stick_and_discharge().await;

    s.fh.advance(RETRY - GIVE_UP).await;
    s.refused_inside_the_window(first_cseq + 1, Class::Normal).await;
    s.serve_the_other_identity().await;
    s.refused_inside_the_window(first_cseq + 2, Class::Emergency).await;
    let past_timer_b = delete_applied + T1 * 64 + PAST_TIMER_B;
    s.fh.advance(past_timer_b - Instant::now()).await;
    s.refused_inside_the_window(first_cseq + 3, Class::Normal).await;
    let late = delete_applied + WINDOW - BEFORE_THE_WINDOW_ENDS;
    s.fh.advance(late - Instant::now()).await;
    s.refused_inside_the_window(first_cseq + 4, Class::Normal).await;

    s.serve_the_identity_past_the_window(delete_applied, first_cseq + 5).await;
    let (stuck, other) = (s.stuck.clone(), s.other.clone());
    s.finish(name, &[(&stuck.0, stuck.1), (&other.0, other.1)]).await;
}

/// b1's 100 is lost on its way to alice, so her Timer A copies of the
/// INVITE go on: the one at 0.5 s reaches b1, the one at 1.5 s, 0.5 s after
/// the delete, reaches b2 under the cookie naming b1. b2 answers that copy
/// the bare 503 at once, never asks its routing and dials no callee: the
/// caller's transaction ends on a final, and the discharge's CDR is the
/// call's only one.
#[tokio::test(start_paused = true)]
async fn a_copy_of_the_discharged_invite_reaching_the_survivor_draws_the_bare_503() {
    let name = "discharged-identity-copy-refused";
    let mut s = Scene::start(name, AliceHears::NoTrying).await;
    let sent = Instant::now();
    let mut first = s.invite(s.stuck(), Party::Caller.first_cseq()).await;
    s.fh.advance(sent + FIRST_COPY - Instant::now()).await;
    first.retransmit().await;
    s.fh.advance(sent + READ_STUCK - Instant::now()).await;
    s.discharge().await;

    s.fh.advance(sent + SECOND_COPY - Instant::now()).await;
    first.retransmit().await;
    s.fh.advance(TRANSIT).await;
    assert_eq!(s.b2.seen.consults_of(s.stuck().0), 0, "b2 never asks the identity's routing");
    assert!(s.b2.seen.stuck_task().is_none(), "b2 does not stick");
    assert_bare(&first.expect(503).await);
    assert_eq!(s.invites_to_bob(), 0, "no callee dialled");
    assert_eq!(s.refused_on_b2(Class::Normal), 1, "counted as a discharged identity");
    let dropped = s.b2.sut.metrics().foreign_invite_copies_dropped_total();
    assert_eq!(dropped, 0, "answered, not dropped as another primary's copy");
    s.finish(name, &[]).await;
}
