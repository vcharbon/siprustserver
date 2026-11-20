//! The answers the layer sends for a peer's copy of a final to its own
//! request, and for a copy of an INVITE it refused: the hop ACK of a non-2xx
//! copy (RFC 3261 §17.1.1.3), an orphan's re-ACK of a 2xx copy, counted per
//! dialog (§13.2.2.4), and the refusal its memo replays (§8.2.7).

use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use call::TxnKind;
use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::{SipMessage, SipRequest, SipResponse};
use sip_retransmit::timers::T1;
use sip_retransmit::Class;
use sip_txn::{CopyKind, IdGen, InviteRefusals, NewCallGate, TransactionConfig};

use super::{answers, burst, copies_every, COPIES};
use crate::common::{self, *};

const TRANSIT: u64 = 5;

/// The dialogs an INVITE client transaction remembers, one per fork (RFC
/// 6026 §7.2): the forks past them share one count.
const DIALOGS_REMEMBERED: usize = 16;

/// The requests of `method` in a drained batch.
fn requests(msgs: &[SipMessage], method: &str) -> usize {
    msgs.iter().filter(|m| matches!(m, SipMessage::Request(r) if r.method() == method)).count()
}

/// The copies of one final its answerer re-sends on `class`'s ladder.
fn resends(class: Class) -> usize {
    usize::try_from(class.resends()).expect("fits")
}

/// A non-2xx final to the layer's INVITE, re-sent at wire speed: the first
/// and the copies Timer G re-sends (§17.2.1) draw the hop ACK, the rest none.
#[tokio::test(start_paused = true)]
async fn a_non_2xx_copied_at_wire_speed_draws_its_hop_ack_on_timer_gs_schedule() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-rejected-final-copies";
    let invite = outbound_request("INVITE", branch);
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.expect("the layer runs");
    elapse_ms(20).await;
    let _ = stack.drain_peer();

    let busy = response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true);
    burst(&stack, &busy, 1 + COPIES).await;
    elapse_ms(20).await;
    let acks = requests(&stack.drain_peer(), "ACK");
    assert_eq!(acks, 1 + resends(Class::InviteServerFinal), "the final and Timer G's re-sends");
    let absorbed = stack.txn.metrics().copies_past_schedule(CopyKind::Non2xx);
    assert_eq!(absorbed, (1 + COPIES - acks) as u64, "each copy absorbed is counted");
}

/// The 2xx `tag` answers the INVITE on `branch` with, from the callee.
fn ok_tagged(branch: &str, call_id: &str, tag: &str) -> Vec<u8> {
    format!(
        "SIP/2.0 200 OK\r\n\
         Via: SIP/2.0/UDP 127.0.0.1:15071;branch={branch}\r\n\
         From: <sip:b2bua@127.0.0.1:15071>;tag=b2bua-b-1\r\n\
         To: <sip:bob@192.0.2.20:5060>;tag={tag}\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 INVITE\r\n\
         Contact: <sip:bob@{PEER}>\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

/// An INVITE on `branch` whose call released its transactions: an orphan.
async fn orphaned(stack: &Stack, call_ref: &str, call_id: &str, branch: &str) {
    let invite = invite_with_cr_lg(call_ref, call_id, branch, "b-1");
    stack.txn.send_request(invite, addr(PEER), TxnKind::Invite).await.expect("the layer runs");
    stack.txn.cancel_txns_for_call(call_ref).await.expect("the layer runs");
    elapse_ms(20).await;
    let _ = stack.drain_peer();
}

/// An orphan's 2xx re-sent at wire speed: the first and the copies §13.3.1.4
/// re-sends draw the dialog's ACK, the rest none; the BYE leaves once.
#[tokio::test(start_paused = true)]
async fn an_orphans_2xx_copied_at_wire_speed_draws_its_ack_on_the_2xx_schedule() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-orphan-copies", "orphan-copies");
    orphaned(&stack, "self|orphan-copies", call_id, branch).await;

    burst(&stack, &ok_tagged(branch, call_id, "fork-a"), 1 + COPIES).await;
    elapse_ms(20).await;
    let out = stack.drain_peer();
    assert_eq!(requests(&out, "ACK"), 1 + resends(Class::Final2xx), "the 2xx and its re-sends");
    assert_eq!(requests(&out, "BYE"), 1, "one BYE");
    let absorbed = stack.txn.metrics().copies_past_schedule(CopyKind::Final2xx);
    assert_eq!(absorbed, (COPIES - resends(Class::Final2xx)) as u64, "each one absorbed counted");
}

/// The 2xx of one fork past the dialogs an orphan remembers.
async fn fork_2xx(stack: &Stack, branch: &str, call_id: &str, fork: usize) {
    stack.inject(&ok_tagged(branch, call_id, &format!("fork-{fork}"))).await;
}

/// The forks of an orphan past the dialogs it remembers share one count:
/// two of them re-sending their 2xx interleaved at wire speed draw one first
/// ACK and the 2xx ladder's re-sends between them, where two remembered forks
/// draw that many each.
#[tokio::test(start_paused = true)]
async fn the_forks_past_an_orphans_dialog_bound_share_one_count() {
    let stack = Stack::build(TRANSIT, 1024, 64).await;
    let (branch, call_id) = ("z9hG4bK-orphan-many-forks", "orphan-many-forks");
    orphaned(&stack, "self|orphan-many-forks", call_id, branch).await;
    for fork in 0..DIALOGS_REMEMBERED {
        fork_2xx(&stack, branch, call_id, fork).await;
    }
    elapse_ms(20).await;
    let remembered = requests(&stack.drain_peer(), "ACK");
    assert_eq!(remembered, DIALOGS_REMEMBERED, "each remembered fork's first 2xx is ACKed");

    for _ in 0..=COPIES {
        for fork in [DIALOGS_REMEMBERED, DIALOGS_REMEMBERED + 1] {
            fork_2xx(&stack, branch, call_id, fork).await;
        }
        elapse_ms(1).await;
    }
    elapse_ms(20).await;
    let past = requests(&stack.drain_peer(), "ACK");
    assert_eq!(past, 1 + resends(Class::Final2xx), "one count for the forks past the bound");
}

/// Two forks' 2xx to one orphan, re-sent interleaved at wire speed: each
/// dialog counts its own copies.
#[tokio::test(start_paused = true)]
async fn each_dialog_of_an_orphan_counts_its_own_2xx_copies() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let (branch, call_id) = ("z9hG4bK-orphan-forks", "orphan-forks");
    orphaned(&stack, "self|orphan-forks", call_id, branch).await;

    for _ in 0..=COPIES {
        for tag in ["fork-a", "fork-b"] {
            stack.inject(&ok_tagged(branch, call_id, tag)).await;
        }
        elapse_ms(1).await;
    }
    elapse_ms(20).await;
    let out = stack.drain_peer();
    let acks_of = |tag: &str| {
        out.iter()
            .filter(|m| {
                matches!(m, SipMessage::Request(r)
                    if r.method() == "ACK" && r.to().tag() == Some(tag))
            })
            .count()
    };
    let owed = 1 + resends(Class::Final2xx);
    assert_eq!((acks_of("fork-a"), acks_of("fork-b")), (owed, owed), "per dialog");
}

/// A stateless refusal under a request-derived To-tag.
fn refuse_503(req: &SipRequest) -> Result<SipResponse, sip_message::generators::GenerateError> {
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(format!("refused-{}", req.call_id().as_str().replace('@', "."))),
            ..Default::default()
        },
    )
}

/// A layer whose new-call gate refuses every new normal INVITE.
async fn refusing() -> Stack {
    let switch = Arc::new(AtomicBool::new(true));
    Stack::build_with_config(
        1,
        1024,
        TransactionConfig {
            id_gen: Arc::new(IdGen::seeded(0xC0FFEE)),
            invite_refusals: Some(InviteRefusals::new(Arc::new(refuse_503))),
            new_call_gate: Some(NewCallGate::new(switch, sip_txn::class_of)),
            ..common::config()
        },
    )
    .await
}

/// A refused INVITE re-sent at wire speed: the refusal and the copies Timer A
/// re-sends draw the 503, the rest none.
#[tokio::test(start_paused = true)]
async fn a_refused_invite_copied_at_wire_speed_draws_its_refusal_on_timer_as_schedule() {
    let stack = refusing().await;
    let invite = inbound_request("INVITE", "z9hG4bK-refused-copies", "refused@copies", None);
    burst(&stack, &invite, 1 + COPIES).await;
    elapse_ms(20).await;
    let refused = answers(&stack.drain_peer(), 503, "INVITE");
    assert_eq!(refused, 1 + resends(Class::InviteClient), "the refusal and Timer A's re-sends");
    let absorbed = stack.txn.metrics().copies_past_schedule(CopyKind::Refused);
    assert_eq!(absorbed, (1 + COPIES - refused) as u64, "each copy absorbed is counted");
}

/// A refused INVITE re-sent every 2 s: each copy inside 64·T1 of the first is
/// answered, and none past it, where the memo of the first refusal ends and
/// the gate would judge the copy again.
#[tokio::test(start_paused = true)]
async fn a_refused_invite_re_sent_past_64_t1_draws_no_refusal_again() {
    let stack = refusing().await;
    let invite = inbound_request("INVITE", "z9hG4bK-refused-past", "refused@past", None);
    let window = 64 * T1;
    let drawn = copies_every(&stack, &invite, 2_000, window + 6_000, 503, "INVITE").await;
    let (inside, past): (Vec<_>, Vec<_>) = drawn.into_iter().partition(|(at, _)| *at < window);
    assert!(inside.iter().all(|(_, n)| *n == 1), "each copy T1 apart is answered: {inside:?}");
    let late: usize = past.iter().filter(|(at, _)| *at > window).map(|(_, n)| n).sum();
    assert_eq!(late, 0, "no copy past 64·T1 of the first draws a refusal: {past:?}");
}
