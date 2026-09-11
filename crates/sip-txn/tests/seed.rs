//! `TransactionLayer::seed` / `reoffer` (ADR-0014 materialisation): the
//! INVITE transactions a call taken over from a peer names are rebuilt here as
//! `Proceeding` and then behave as if this layer had seen the first copy — the
//! client seed ACKs and Timer-D-holds a non-2xx final, the server seed ladders
//! its final on Timer G and answers a CANCEL 200 + 487 — and the datagram that
//! arrived before them is re-offered against them.

mod common;
use common::*;
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};
use sip_retransmit::Class;
use sip_txn::{Reoffer, TimeoutKind, TransactionEvent, TxnSeed};

const TRANSIT: u64 = 5;
const CALL_REF: &str = "w0|seeded-call@10.0.0.9|atag";
/// The To-tag the dead node's provisional gave the caller's early dialog.
const SEED_TO_TAG: &str = "seed-to-tag";

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

/// The `Message` events in a drained batch, as `(is_response, status_or_0,
/// matched_client_txn)`.
fn messages(events: &[TransactionEvent]) -> Vec<(bool, u16, bool)> {
    events
        .iter()
        .filter_map(|e| match e {
            TransactionEvent::Message { message, matched_client_txn, .. } => {
                Some(match message.as_ref() {
                    SipMessage::Response(r) => (true, r.status(), *matched_client_txn),
                    SipMessage::Request(_) => (false, 0, *matched_client_txn),
                })
            }
            _ => None,
        })
        .collect()
}

fn cancelled(events: &[TransactionEvent]) -> bool {
    events.iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. }))
}

/// The To-tag of the first `status` response in a drained batch.
fn to_tag_of(msgs: &[SipMessage], status: u16) -> Option<String> {
    msgs.iter().find_map(|m| match m {
        SipMessage::Response(r) if r.status() == status => r.to().tag().map(str::to_string),
        _ => None,
    })
}

fn request_bytes(raw: &[u8]) -> SipRequest {
    match CustomParser::new().parse(raw).expect("parse request") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

/// A client INVITE seed for the b-leg INVITE this node's peer sent on
/// `branch`, attributed to `CALL_REF` through its Via.
fn client_seed(branch: &str, call_id: &str) -> TxnSeed {
    TxnSeed::ClientInvite {
        invite: invite_with_cr_lg(CALL_REF, call_id, branch, "b-1"),
        dest: addr(PEER),
    }
}

/// A server INVITE seed for the a-leg INVITE the peer admitted on `branch` and
/// already answered provisionally under `SEED_TO_TAG`, carrying the rebuilt
/// request when `with_request`.
fn server_seed(branch: &str, call_id: &str, with_request: bool) -> TxnSeed {
    TxnSeed::ServerInvite {
        branch: branch.to_string(),
        call_id: call_id.to_string(),
        from_tag: "caller-tag".to_string(),
        to_tag: Some(SEED_TO_TAG.to_string()),
        leg_id: Some("a".to_string()),
        original_request: with_request
            .then(|| request_bytes(&inbound_request("INVITE", branch, call_id, None))),
    }
}

// ── The client seed ─────────────────────────────────────────────────────────

/// A seeded client INVITE is counted for its call, ACKs a 486 hop-by-hop, is
/// held in Completed for Timer D so a retransmitted 486 is re-ACKed and
/// absorbed, and is gone after Timer D.
#[tokio::test(start_paused = true)]
async fn a_seeded_client_invite_acks_a_486_and_holds_timer_d() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-seed-c";
    let seeded = stack.txn.seed(CALL_REF, vec![client_seed(branch, "seed-c")]).await.unwrap();
    assert_eq!(seeded, 1);
    assert_eq!(stack.txn.metrics().txn_seeded(), 1);
    assert_eq!(active(&stack), 1, "the seed is a resident transaction");
    assert_eq!(
        stack.txn.active_txn_count_for_call(CALL_REF).await.unwrap(),
        1,
        "a seed is visible to the self-release count (ADR-0014)"
    );
    elapse_ms(3_000).await;
    assert!(stack.drain_peer().is_empty(), "a seed re-sends nothing: no Timer A ladder (D9)");

    stack.inject(&response_bytes(486, "Busy Here", "INVITE", branch, "seed-c", true)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "ACK"),
        1,
        "the layer ACKs the final (§17.1.1.3)"
    );
    assert_eq!(
        messages(&stack.drain_events()),
        vec![(true, 486, true)],
        "the final reaches the consumer once, matched"
    );
    assert_eq!(active(&stack), 1, "Completed, holding Timer D");

    stack.inject(&response_bytes(486, "Busy Here", "INVITE", branch, "seed-c", true)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "ACK"),
        1,
        "a retransmitted final is re-ACKed (§17.1.1.2)"
    );
    assert!(stack.drain_events().is_empty(), "and absorbed");

    elapse_ms(33_000).await;
    assert_eq!(active(&stack), 0, "Timer D deletes the seed");
}

/// A seeded client INVITE carries the INVITE bound, not Timer B: it gives up
/// with a `Transaction` timeout attributed to its call after
/// `invite_initial_timeout_ms`, and no earlier.
#[tokio::test(start_paused = true)]
async fn a_seeded_client_invite_gives_up_on_the_invite_bound() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack.txn.seed(CALL_REF, vec![client_seed("z9hG4bK-seed-bound", "seed-bound")]).await.unwrap();

    elapse_ms(33_000).await;
    assert!(
        stack.drain_events().is_empty(),
        "past Timer B the seed still waits (it is Proceeding)"
    );

    elapse_ms(sip_txn::timers::INVITE_INITIAL_TIMEOUT).await;
    let events = stack.drain_events();
    let timeout = events.iter().find_map(|e| match e {
        TransactionEvent::Timeout { call_ref, leg_id, method, kind, .. } => {
            Some((call_ref.clone(), leg_id.clone(), method.clone(), *kind))
        }
        _ => None,
    });
    assert_eq!(
        timeout,
        Some((
            Some(CALL_REF.to_string()),
            Some("b-1".to_string()),
            Some("INVITE".to_string()),
            TimeoutKind::Transaction
        )),
        "the bound fires as the configured INVITE bound, for the seed's call and leg"
    );
    assert_eq!(active(&stack), 0);
}

// ── The server seed ─────────────────────────────────────────────────────────

/// A seeded server INVITE ladders a non-2xx final on Timer G until the ACK on
/// its branch, then falls silent.
#[tokio::test(start_paused = true)]
async fn a_seeded_server_invite_ladders_a_final_on_timer_g_until_the_ack() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-seed-s";
    let call_id = "seed-s";
    assert_eq!(
        stack.txn.seed(CALL_REF, vec![server_seed(branch, call_id, false)]).await.unwrap(),
        1
    );
    assert_eq!(stack.txn.active_txn_count_for_call(CALL_REF).await.unwrap(), 1);

    let resp = parse_response(&response_bytes(486, "Busy Here", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 1, "486 sent once");
    assert_eq!(active(&stack), 1, "Completed on the seed (Timer G/H)");

    elapse_ms(600).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 1, "Timer G re-sends at T1");
    assert_eq!(stack.txn.metrics().retransmits(Class::InviteServerFinal), 1);
    assert_eq!(
        stack.txn.metrics().server_final_unseen_branch(),
        0,
        "the branch was seen: it was seeded"
    );

    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(active(&stack), 0, "the ACK on the INVITE's branch terminates the seed");
    elapse_ms(5_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 0, "no retransmit after the ACK");
    let _ = stack.drain_events();
}

/// A seeded server INVITE carrying its request answers a CANCEL 200 + 487 under
/// the To-tag the seed names — the one the peer's early dialog already holds —
/// and emits `Cancelled`, as for any INVITE this layer admitted; one seeded
/// without the request cannot compose the 487 and hands the CANCEL up instead.
#[tokio::test(start_paused = true)]
async fn a_seeded_server_invite_with_its_request_answers_a_cancel() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-seed-cxl";
    let call_id = "seed-cxl";
    stack.txn.seed(CALL_REF, vec![server_seed(branch, call_id, true)]).await.unwrap();

    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1, "200 to the CANCEL");
    assert_eq!(count_responses(&out, 487), 1, "487 to the seeded INVITE");
    assert_eq!(
        to_tag_of(&out, 487).as_deref(),
        Some(SEED_TO_TAG),
        "the 487 rides the seed's To-tag (§17.2.1)"
    );
    assert_eq!(to_tag_of(&out, 200).as_deref(), Some(SEED_TO_TAG), "so does the CANCEL's 200");
    let events = stack.drain_events();
    assert!(cancelled(&events), "Cancelled reaches the consumer");
    assert!(messages(&events).is_empty(), "the CANCEL itself does not");
    assert_eq!(active(&stack), 1, "Completed on the 487 until its ACK");

    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(active(&stack), 0);

    // Without the request: the CANCEL is unanswerable here and is the TU's.
    let bare = "z9hG4bK-seed-bare";
    stack.txn.seed(CALL_REF, vec![server_seed(bare, "seed-bare", false)]).await.unwrap();
    stack.inject(&inbound_request("CANCEL", bare, "seed-bare", None)).await;
    elapse_ms(60).await;
    assert!(stack.drain_peer().is_empty(), "no 200, no 487 without the request");
    let events = stack.drain_events();
    assert!(!cancelled(&events));
    assert_eq!(messages(&events), vec![(false, 0, false)], "the CANCEL is handed up");
}

// ── Occupancy and the unseen branch ─────────────────────────────────────────

/// A seed whose branch already holds a transaction is skipped and counted; the
/// occupant is untouched.
#[tokio::test(start_paused = true)]
async fn a_seed_on_an_occupied_branch_is_skipped_and_counted() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-occupied";
    stack.inject(&inbound_request("INVITE", branch, "occupied", None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();
    assert_eq!(active(&stack), 1);

    let seeded = stack
        .txn
        .seed(
            CALL_REF,
            vec![server_seed(branch, "occupied", true), client_seed("z9hG4bK-free", "occupied")],
        )
        .await
        .unwrap();
    assert_eq!(seeded, 1, "only the free branch is seeded");
    assert_eq!(stack.txn.metrics().txn_seeded(), 1);
    assert_eq!(stack.txn.metrics().txn_seed_skipped(), 1);
    assert_eq!(active(&stack), 2);
    assert_eq!(
        stack.txn.active_txn_count_for_call(CALL_REF).await.unwrap(),
        1,
        "the occupant keeps its own attribution (none here); only the seed counts for the call"
    );
}

/// A non-2xx INVITE final on a branch no transaction holds leaves raw, once,
/// builds nothing and bumps `server_final_unseen_branch` (D14); a 2xx on such a
/// branch leaves raw and counts nothing.
#[tokio::test(start_paused = true)]
async fn a_final_on_an_unseen_branch_leaves_raw_once_and_is_counted() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let resp = parse_response(&response_bytes(
        486,
        "Busy Here",
        "INVITE",
        "z9hG4bK-unseen",
        "unseen",
        true,
    ));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 486), 1, "the 486 leaves once");
    assert_eq!(active(&stack), 0, "and builds no transaction");
    assert_eq!(stack.txn.metrics().server_final_unseen_branch(), 1);

    elapse_ms(8_000).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 486),
        0,
        "no Timer G ladder without a transaction"
    );
    assert_eq!(stack.txn.metrics().retransmits(Class::InviteServerFinal), 0);

    let ok =
        parse_response(&response_bytes(200, "OK", "INVITE", "z9hG4bK-unseen2xx", "unseen", true));
    stack.txn.send_response(ok, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1, "the 2xx leaves raw");
    assert_eq!(stack.txn.metrics().server_final_unseen_branch(), 1, "a 2xx is not counted");
}

// ── Re-offer ────────────────────────────────────────────────────────────────

/// A 486 that arrived before the seed existed is re-offered after it: the
/// seeded client transaction ACKs it, goes Completed on Timer D and re-emits it
/// with `matched_client_txn = true`. Re-offered against no transaction it is
/// `Unmatched`: nothing sent, nothing emitted.
#[tokio::test(start_paused = true)]
async fn a_reoffered_final_is_taken_by_the_seed_it_arrived_before() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-reoffer";
    let raw = response_bytes(486, "Busy Here", "INVITE", branch, "reoffer", true);
    stack.inject(&raw).await;
    elapse_ms(60).await;
    assert_eq!(
        messages(&stack.drain_events()),
        vec![(true, 486, false)],
        "the first arrival matched nothing"
    );
    assert!(stack.drain_peer().is_empty(), "and drew no ACK");
    let final_486 = CustomParser::new().parse(&raw).expect("parse");

    assert_eq!(
        stack.txn.reoffer(final_486.clone(), addr(PEER)).await.unwrap(),
        Reoffer::Unmatched,
        "re-offered against an empty map: unmatched"
    );
    elapse_ms(60).await;
    assert!(stack.drain_peer().is_empty(), "an unmatched re-offer sends nothing");
    assert!(stack.drain_events().is_empty(), "and emits nothing");

    stack.txn.seed(CALL_REF, vec![client_seed(branch, "reoffer")]).await.unwrap();
    assert_eq!(stack.txn.reoffer(final_486, addr(PEER)).await.unwrap(), Reoffer::Matched);
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "the seed ACKs the re-offered final");
    assert_eq!(messages(&stack.drain_events()), vec![(true, 486, true)], "re-emitted, matched");
    assert_eq!(active(&stack), 1, "Completed on Timer D");
}

/// A CANCEL that arrived before the seed existed is re-offered after it: the
/// seeded server INVITE answers 200 + 487 and emits `Cancelled`. Re-offered
/// against no active INVITE it is `Unmatched`, and the layer stays silent — the
/// 481 is the consumer's.
#[tokio::test(start_paused = true)]
async fn a_reoffered_cancel_is_answered_by_the_seed_it_arrived_before() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-reoffer-cxl";
    let call_id = "reoffer-cxl";
    let raw = inbound_request("CANCEL", branch, call_id, None);
    stack.inject(&raw).await;
    elapse_ms(60).await;
    assert_eq!(
        messages(&stack.drain_events()),
        vec![(false, 0, false)],
        "the CANCEL was handed up"
    );
    let cancel = CustomParser::new().parse(&raw).expect("parse");

    assert_eq!(stack.txn.reoffer(cancel.clone(), addr(PEER)).await.unwrap(), Reoffer::Unmatched);
    elapse_ms(60).await;
    assert!(stack.drain_peer().is_empty() && stack.drain_events().is_empty());

    stack.txn.seed(CALL_REF, vec![server_seed(branch, call_id, true)]).await.unwrap();
    assert_eq!(stack.txn.reoffer(cancel, addr(PEER)).await.unwrap(), Reoffer::Matched);
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1);
    assert_eq!(count_responses(&out, 487), 1);
    assert!(cancelled(&stack.drain_events()));
}

/// A request re-offered on a branch a server transaction already holds is the
/// retransmission it is: the cached response is replayed and it is `Matched`;
/// a bare seed holding no response yet composes the 100 Trying it owes the
/// retransmission (§17.2.1) and caches it for the next; on a branch nothing
/// holds it is `Unmatched` and no transaction is built.
#[tokio::test(start_paused = true)]
async fn a_reoffered_request_is_absorbed_by_the_transaction_on_its_branch() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-reoffer-inv";
    let raw = inbound_request("INVITE", branch, "reoffer-inv", None);
    stack.inject(&raw).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "the first arrival drew the auto-100");
    let _ = stack.drain_events();
    let invite = CustomParser::new().parse(&raw).expect("parse");

    assert_eq!(stack.txn.reoffer(invite, addr(PEER)).await.unwrap(), Reoffer::Matched);
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 100),
        1,
        "the cached 100 is replayed (§17.2.1)"
    );
    assert!(stack.drain_events().is_empty(), "a retransmission surfaces nothing");
    assert_eq!(active(&stack), 1);

    // A bare seed (a relayed re-INVITE rebuilt without its request) has no
    // response cached: the retransmission draws a 100 composed now, the next
    // one the cached copy, and neither surfaces.
    let bare = "z9hG4bK-reoffer-bare";
    let bare_raw = inbound_request("INVITE", bare, "reoffer-bare", Some("peer-tag"));
    stack.txn.seed(CALL_REF, vec![server_seed(bare, "reoffer-bare", false)]).await.unwrap();
    let retransmission = CustomParser::new().parse(&bare_raw).expect("parse");
    assert_eq!(stack.txn.reoffer(retransmission, addr(PEER)).await.unwrap(), Reoffer::Matched);
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 100),
        1,
        "a Proceeding seed owes the retransmission a provisional (§17.2.1)"
    );
    assert!(stack.drain_events().is_empty());
    stack.inject(&bare_raw).await;
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 100),
        1,
        "every later retransmission draws the cached 100"
    );
    assert!(stack.drain_events().is_empty());
    assert_eq!(active(&stack), 2);

    let other = CustomParser::new()
        .parse(&inbound_request("INVITE", "z9hG4bK-nobody", "nobody", None))
        .expect("parse");
    assert_eq!(stack.txn.reoffer(other, addr(PEER)).await.unwrap(), Reoffer::Unmatched);
    elapse_ms(60).await;
    assert!(stack.drain_peer().is_empty(), "no 100 for a request no transaction holds");
    assert_eq!(active(&stack), 2, "and no transaction is built for it");
}
