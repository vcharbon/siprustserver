//! An acting backup takes over a call from an in-dialog request whose
//! Request-URI an element on the path rewrote: the callee's request reaches
//! the survivor with no `callRef` of ours, so the call is found through the
//! replicated SIP index, and the request is the outgoing leg's.
//!
//! ```text
//!   alice ──▶ LB ──▶ primary ──▶ LB ──▶ rewriter ──▶ bob
//!                    ✗ crash
//!   bob BYE / re-INVITE ──▶ rewriter (Request-URI replaced) ──▶ LB ──▶ backup
//! ```
//!
//! A BYE ends the call on the backup. A re-INVITE whose handler dies on the
//! takeover copy is answered 481 when the copy is released (RFC 3261
//! §12.2.2): its server transaction is the copy's, found by dialog identity.
//! Either way the primary departed, so the backup discharges the call it
//! ends; the primary reboots within its budget, and the call ends with its
//! one CDR.
//!
//! Beyond the failover sweep: a callee-side proxy rewriting the Request-URI of
//! a BYE and of a re-INVITE.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua::rules::{
    Match, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, ServiceDef, ServiceSeed,
    SERVICE_LAYER,
};
use failover_harness::{
    assert_call_fully_released, total_cdrs_for, FailoverHarness, ProxySut, ReplicatedB2buaSut,
    WorkerHealth,
};
use scenario_harness::{Agent, Dialog, Proxy};
use sip_message::header::Uri;
use sip_message::{SipRequest, SipStr};

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const REWRITER: &str = "127.0.0.1:5075";
const LB: &str = "127.0.0.1:5080";
const B1: &str = "127.0.0.1:5091";
const B2: &str = "127.0.0.1:5092";
/// What the rewriter aims every in-dialog request at: no user, no parameter
/// of the B2BUA's Contact.
const REWRITTEN: &str = "sip:b2bua.invalid";

fn addr(a: &str) -> SocketAddr {
    a.parse().unwrap()
}

/// The primary the load balancer's cookie names, on whichever recorded route
/// carries it (the rewriter's own sits on top).
fn primary_of(invite: &SipRequest) -> String {
    let recorded = invite.record_route_set().expect("readable Record-Route");
    let primary = recorded
        .iter()
        .find_map(|rr| rr.uri().param("w_pri").and_then(|v| v.as_str()).map(str::to_string));
    primary.expect("the load balancer recorded its cookie")
}

fn pair<'a>(
    w_b1: &'a mut ReplicatedB2buaSut,
    w_b2: &'a mut ReplicatedB2buaSut,
    primary_ord: &str,
) -> (&'a mut ReplicatedB2buaSut, &'a mut ReplicatedB2buaSut) {
    if primary_ord == "b1" {
        (w_b1, w_b2)
    } else {
        (w_b2, w_b1)
    }
}

/// `req` as the rewriter forwards it: aimed at [`REWRITTEN`].
fn rewritten(req: SipRequest) -> SipRequest {
    let uri = Uri::parse(&SipStr::owned(REWRITTEN)).expect("a SIP URI");
    req.thaw().with_uri(uri).freeze().expect("a request stays complete")
}

fn no_init(_: &RuleCall) -> Option<ServiceSeed> {
    None
}

/// Outranks the core re-INVITE relay and panics on an INVITE in a dialog:
/// the handler dies before any answer.
fn panic_on_reinvite(ctx: &RuleContext) -> Option<RuleHandleResult> {
    if ctx.request().is_some_and(|r| r.to().tag().is_some()) {
        panic!("probe: handler panic on re-INVITE");
    }
    None
}

fn panic_on_reinvite_services() -> Vec<ServiceDef> {
    fn rules() -> Vec<RuleDefinition> {
        vec![RuleDefinition::core(
            "probe-panic-on-reinvite",
            SERVICE_LAYER,
            &[],
            Match::request().method("INVITE"),
            panic_on_reinvite,
        )]
    }
    vec![ServiceDef { id: "probe-panic-reinvite", init: no_init, rules }]
}

/// An established call whose callee side runs through the rewriter, its
/// primary crashed and read dead, its backup told the primary left.
struct Crashed {
    fh: FailoverHarness,
    alice: Agent,
    bob: Agent,
    rewriter: Proxy,
    proxy: ProxySut,
    w_b1: ReplicatedB2buaSut,
    w_b2: ReplicatedB2buaSut,
    primary_ord: String,
    call_ref: String,
    _alice_dialog: Dialog,
    bob_dialog: Dialog,
}

/// INVITE → 200 → ACK, every callee-side message through the rewriter, the
/// call replicated, then the primary crashes, on workers running `services`.
async fn crashed(name: &str, services: fn() -> Vec<ServiceDef>) -> Crashed {
    let mut fh = FailoverHarness::new(name, &["b1", "b2"]).with_worker_services(services);
    let alice = fh.agent("alice", ALICE).await;
    let bob = fh.agent("bob", BOB).await;
    let rewriter = fh.scripted_proxy("rewriter", REWRITER).await;
    let proxy: ProxySut = fh.spawn_proxy(LB, &[("b1", addr(B1)), ("b2", addr(B2))]).await;
    let rewriter_at = ("127.0.0.1", 5075);
    let lb_at = ("127.0.0.1", 5080);
    let mut w_b1 = fh.spawn_worker("b1", "b1", B1, &["b2"], rewriter_at, lb_at).await;
    let mut w_b2 = fh.spawn_worker("b2", "b2", B2, &["b1"], rewriter_at, lb_at).await;
    fh.advance(Duration::from_millis(500)).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(proxy.addr()).send().await;
    rewriter.forward_request(addr(BOB)).await;
    let mut uas = bob.receive("INVITE").await;
    let primary_ord = primary_of(uas.request());
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    rewriter.forward_response(addr(LB)).await;
    call.expect(200).await;
    let alice_dialog = call.ack().await;
    rewriter.forward_request(addr(BOB)).await;
    bob.receive("ACK").await;
    let bob_dialog = uas.dialog();

    fh.advance(Duration::from_millis(500)).await;
    let mut call_ref = String::new();
    {
        let (_, backup) = pair(&mut w_b1, &mut w_b2, &primary_ord);
        for _ in 0..50 {
            if let Some(found) = backup.scan_one_backed_up(&primary_ord).await {
                call_ref = found;
                break;
            }
            fh.advance(Duration::from_millis(100)).await;
        }
    }
    assert!(!call_ref.is_empty(), "the backup holds the replicated call");

    {
        let (primary, backup) = pair(&mut w_b1, &mut w_b2, &primary_ord);
        primary.crash();
        proxy.set_health(&primary_ord, WorkerHealth::Dead);
        backup.simulate_peer_removed(&primary_ord);
    }
    fh.advance(Duration::from_millis(300)).await;
    Crashed {
        fh,
        alice,
        bob,
        rewriter,
        proxy,
        w_b1,
        w_b2,
        primary_ord,
        call_ref,
        _alice_dialog: alice_dialog,
        bob_dialog,
    }
}

/// The primary returns within its budget; the call ends fully released on
/// both nodes with its one CDR, which is returned.
async fn primary_returns(c: &mut Crashed) -> b2bua::cdr::CdrRecord {
    c.fh.advance(Duration::from_secs(60)).await;
    {
        let (primary, backup) = pair(&mut c.w_b1, &mut c.w_b2, &c.primary_ord);
        c.fh.mark(&c.primary_ord, None, "reboot", "restart empty, higher gen, new pod IP");
        let new_addr = primary.reboot().await;
        c.proxy.set_address(&c.primary_ord, new_addr);
        c.fh.note_worker_rebound(&c.primary_ord, new_addr);
        backup.simulate_peer_added(&c.primary_ord);
        for _ in 0..120 {
            c.fh.advance(Duration::from_millis(500)).await;
            if primary.is_ready() {
                break;
            }
        }
        assert!(primary.is_ready(), "the rebooted primary became ready");
        c.proxy.set_health(&c.primary_ord, WorkerHealth::Alive);
    }
    c.fh.advance(Duration::from_secs(10)).await;

    let (w_b1, w_b2, call_ref) = (&c.w_b1, &c.w_b2, &c.call_ref);
    let _ =
        c.fh.settle_terminal(async || {
            w_b1.memory_clean()
                && w_b2.memory_clean()
                && !w_b1.holds_any_trace(call_ref).await
                && !w_b2.holds_any_trace(call_ref).await
        })
        .await;
    c.fh.linger_peers(&[&c.alice, &c.bob], Duration::from_secs(3)).await;
    assert_call_fully_released(&[w_b1, w_b2], call_ref).await;
    assert_eq!(total_cdrs_for(&[w_b1, w_b2], call_ref), 1, "the call has its one CDR");
    [w_b1, w_b2]
        .iter()
        .flat_map(|n| n.cdr_records())
        .find(|r| &r.call_ref == call_ref)
        .expect("the one CDR")
}

#[tokio::test(start_paused = true)]
async fn a_callee_bye_without_our_request_uri_reaches_the_backup_on_the_outgoing_leg() {
    let mut c = crashed("takeover-request-uri-rewritten", Vec::new).await;

    // bob hangs up; the rewriter aims the BYE at a URI that is not ours.
    let mut bye = c.bob_dialog.bye().await;
    c.rewriter.forward_request_altered(addr(LB), rewritten).await;
    c.alice.receive("BYE").await.respond(200, "OK").await;
    c.rewriter.forward_response(addr(BOB)).await;
    bye.expect(200).await;

    let cdr = primary_returns(&mut c).await;
    let bye = cdr
        .events
        .iter()
        .find(|e| e.event_type == call::CdrEventType::Bye)
        .unwrap_or_else(|| panic!("the BYE is recorded: {:?}", cdr.events));
    assert_eq!(cdr.b_legs.len(), 1, "{cdr:?}");
    assert_eq!(bye.leg_id, cdr.b_legs[0].leg_id, "the callee's BYE is the outgoing leg's");
    let termination = cdr.termination.as_ref().expect("the record names who ended the call");
    assert_eq!(termination.by_leg.as_deref(), Some(cdr.b_legs[0].leg_id.as_str()));
}

/// bob's re-INVITE takes the call over on the backup through the replica's
/// SIP index and its handler dies before any answer; the reaper's BYEs end
/// both dialogs and the copy is released with the re-INVITE unanswered. The
/// release answers it 481 through its server transaction (RFC 3261 §12.2.2),
/// which absorbs bob's ACK, and the copy is reaped.
#[tokio::test(start_paused = true)]
async fn a_callee_reinvite_without_our_request_uri_whose_handler_died_is_answered_481() {
    let name = "takeover-request-uri-rewritten-reinvite";
    let mut c = crashed(name, panic_on_reinvite_services).await;

    let mut reinvite = c.bob_dialog.reinvite(Some(ANSWER)).await;
    let forwarded = c.rewriter.forward_request_altered(addr(LB), rewritten).await;
    c.alice.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    c.rewriter.forward_request(addr(BOB)).await;
    c.bob.receive_tolerating("BYE", &["OPTIONS"]).await.respond(200, "OK").await;
    c.rewriter.forward_response(addr(LB)).await;
    c.rewriter.forward_response(addr(BOB)).await;
    reinvite
        .try_expect(481)
        .await
        .expect("the released copy answers the re-INVITE its handler never answered");
    let branch = forwarded.top_via().branch().expect("the rewriter's branch").to_string();
    c.rewriter.forward_request_on_branch(addr(LB), &branch).await;

    let (_, backup) = pair(&mut c.w_b1, &mut c.w_b2, &c.primary_ord);
    let backup = &*backup;
    assert!(
        c.fh.settle_terminal(async || !backup.serves(&c.call_ref)).await,
        "the reaper tears the copy down and the backup sheds it",
    );
    assert_eq!(backup.metrics().handler_panics_total(), 1);

    let cdr = primary_returns(&mut c).await;
    assert_eq!(cdr.b_legs.len(), 1, "{cdr:?}");
}
