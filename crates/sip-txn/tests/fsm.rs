//! Native FSM coverage for behaviours the source only exercises through full
//! B2BUA scenarios (which depend on the unported call + rules layers). These
//! pin the RFC 3261 §17 mechanics directly at the transaction-layer seam:
//! retransmission cadence, Timer B timeout, CANCEL→200+487, ACK absorption,
//! auto-ACK for non-2xx client finals, and cached-response retransmission.
//!
//! Authored (not migrated) — see MIGRATION_STATUS §Slice 4.

mod common;
use common::*;
use sip_message::SipMessage;
use sip_retransmit::Class;
use sip_txn::timers::TIMER_I;
use sip_txn::{RetransmitRow, TransactionEvent, TxnKind};

const TRANSIT: u64 = 5;

fn active(stack: &Stack) -> usize {
    stack.txn.metrics().active_transactions()
}

fn has_message_request(events: &[TransactionEvent], method: &str) -> bool {
    events.iter().any(|e| match e {
        TransactionEvent::Message { message, .. } => {
            matches!(message.as_ref(), SipMessage::Request(r) if r.method() == method)
        }
        _ => false,
    })
}

// ── Client retransmission (Timer A) ─────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn client_retransmits_on_timer_a_cadence() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-rtx"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();

    // By 2 s the peer has seen the initial send + retransmits at 500 ms and
    // 1500 ms (the source's doubling cadence) = 3 INVITEs.
    elapse_ms(2_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 3);
    // Each rung is counted under the ladder that paced it and the request it
    // repeated — the original send is not a rung.
    assert_eq!(stack.txn.metrics().retransmits(Class::InviteClient), 2, "two Timer A rungs");
    assert_eq!(
        stack.txn.metrics().retransmit_rows(),
        vec![RetransmitRow { ladder: "invite-client", method: "INVITE", code: None, count: 2 }],
    );
}

#[tokio::test(start_paused = true)]
async fn provisional_response_stops_retransmit() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-prov";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();

    elapse_ms(700).await; // initial + retransmit @500
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 2);

    // A 100 Trying on the matching branch cancels the retransmit timer.
    stack.inject(&response_bytes(100, "Trying", "INVITE", branch, "prov-call", false)).await;
    elapse_ms(3_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 0, "no retransmit after 100 Trying");
}

// ── Same-branch displacement releases the replaced txn's timers ─────────────

/// A second `send_request` reusing a live Via branch DISPLACES the first txn.
/// The displaced txn's retransmit/Timer-B entries must be physically removed in
/// lockstep — otherwise they linger in the shared `DelayQueue` keyed by the same
/// branch string and fire against the REPLACEMENT, forking its retransmit chain
/// (and, once their freed slab slots are reused, aliasing unrelated timers — the
/// CLAUDE.md no-generation `Key` hazard). Observable here as a doubled retransmit.
#[tokio::test(start_paused = true)]
async fn same_branch_displacement_does_not_fork_retransmits() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-displace";

    // First INVITE on `branch` → immediate send + retransmit armed @500 ms.
    stack
        .txn
        .send_request(
            invite_with_cr_lg("self|old", "cid-old", branch, "b-1"),
            addr(PEER),
            TxnKind::Invite,
        )
        .await
        .unwrap();
    // Second INVITE reusing `branch` → displaces the first; its orphan retransmit
    // must be cancelled, not left to fire against this txn.
    stack
        .txn
        .send_request(
            invite_with_cr_lg("self|new", "cid-new", branch, "b-1"),
            addr(PEER),
            TxnKind::Invite,
        )
        .await
        .unwrap();
    assert_eq!(active(&stack), 1, "displaced, not doubled");

    // Two initial sends + exactly ONE retransmit (the live txn's, @500 ms) by
    // 700 ms. A leaked orphan retransmit would double the @500 ms fire → 4.
    elapse_ms(700).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "INVITE"),
        3,
        "one retransmit chain, not a forked pair"
    );
}

/// A non-INVITE client transaction CONTINUES retransmitting after a provisional
/// (RFC 3261 §17.1.2.2) — only INVITE stops — but the pacing changes: a Timer-E
/// fire in Proceeding re-arms at exactly T2, not the Trying ladder's doubling.
/// The provisional does not touch the already-armed timer (that fire runs to
/// completion at its Trying-ladder deadline); it is the fire itself, landing in
/// Proceeding, that resets to T2. Timer F is untouched: the 64·T1 bound still
/// ends the transaction.
#[tokio::test(start_paused = true)]
async fn non_invite_timer_e_resets_to_t2_on_a_proceeding_fire() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-bye-rtx";
    stack
        .txn
        .send_request(outbound_request("BYE", branch), addr(PEER), TxnKind::NonInvite)
        .await
        .unwrap();
    elapse_ms(20).await;
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 1, "initial send");

    // Trying: first Timer-E fire at T1 (+500 ms), pinned on both sides.
    elapse_ms(460).await; // t=480
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 0, "quiet before the T1 deadline");
    elapse_ms(40).await; // t=520
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 1, "one retransmit at T1");

    // The provisional arrives with the 2*T1 fire (+1500 ms) already armed.
    stack.inject(&response_bytes(100, "Trying", "BYE", branch, "bye-rtx-call", false)).await;
    elapse_ms(20).await; // t=540

    // The pending fire runs to completion at +1500 ms: entering Proceeding
    // neither stops Timer E nor eagerly re-arms it at T2 (which would leave
    // this window quiet and fire near +4525 ms instead).
    elapse_ms(940).await; // t=1480
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 0, "quiet before the 2*T1 deadline");
    elapse_ms(40).await; // t=1520
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 1, "the armed 2*T1 fire completes");

    // That fire landed in Proceeding, so Timer E resets to T2: the next fire is
    // at +5500 ms. Continued Trying-ladder doubling would fire at +3500 ms.
    elapse_ms(3940).await; // t=5460
    assert_eq!(
        count_requests(&stack.drain_peer(), "BYE"),
        0,
        "no Trying-ladder doubling fire in Proceeding"
    );
    elapse_ms(80).await; // t=5540
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 1, "next fire lands T2 after");
    // The two Trying-paced rungs (T1 and 2·T1, the second landing in
    // Proceeding) and the first flat-T2 one are counted under the class that
    // paced each.
    assert_eq!(stack.txn.metrics().retransmits(Class::NonInviteClient), 2);
    assert_eq!(stack.txn.metrics().retransmits(Class::NonInviteProceeding), 1);
    assert_eq!(
        stack.txn.metrics().retransmit_rows(),
        vec![
            RetransmitRow { ladder: "non-invite-client", method: "BYE", code: None, count: 2 },
            RetransmitRow { ladder: "non-invite-proceeding", method: "BYE", code: None, count: 1 },
        ],
    );

    // T2 plateau: the following fire is another T2 later (+9500 ms).
    elapse_ms(3940).await; // t=9480
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 0, "quiet before the T2 plateau fire");
    elapse_ms(40).await; // t=9520
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 1, "T2 plateau fire");

    // Timer F still bounds the transaction: fires at +13.5 s, +17.5 s, +21.5 s,
    // +25.5 s, +29.5 s, then the 64*T1 = 32 s bound emits Timeout and the txn
    // is gone — silence after.
    elapse_ms(23_480).await; // t=33000
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 5, "T2 plateau to the Timer F bound");
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer F emits Timeout"
    );
    assert_eq!(active(&stack), 0, "the txn is reaped at Timer F");
    elapse_ms(8_000).await;
    assert_eq!(count_requests(&stack.drain_peer(), "BYE"), 0, "silent past Timer F");
}

/// A CANCEL fed through `send_request` reusing the INVITE's branch (RFC 3261
/// §9.1) goes raw once the branch has a provisional and must NOT displace the
/// live INVITE client txn at that shared branch — no second, never-completing
/// CANCEL txn is created. (The pre-provisional hold itself is pinned in
/// `cancel_hold.rs`.)
#[tokio::test(start_paused = true)]
async fn send_request_cancel_is_raw_and_does_not_displace_the_invite() {
    let stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-shared";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    assert_eq!(active(&stack), 1);
    // A provisional lands first, so the CANCEL below owes no §9.1 wait.
    stack
        .inject(&response_bytes(180, "Ringing", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(60).await;
    let _ = stack.drain_peer(); // the initial INVITE

    // CANCEL reusing the INVITE's branch through send_request → routed raw.
    stack
        .txn
        .send_request(outbound_request("CANCEL", branch), addr(PEER), TxnKind::NonInvite)
        .await
        .unwrap();
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "CANCEL"), 1, "CANCEL sent raw");
    assert_eq!(active(&stack), 1, "no CANCEL txn created at the shared branch");

    // Positive proof the INVITE txn survived (a same-branch displacement also
    // leaves map size 1): only the intact INVITE client txn — holding its
    // `original_request` — can auto-ACK the 487 final (§17.1.1.3).
    stack
        .inject(&response_bytes(
            487,
            "Request Terminated",
            "INVITE",
            branch,
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(60).await;
    assert_eq!(
        count_requests(&stack.drain_peer(), "ACK"),
        1,
        "auto-ACK of the 487 proves the INVITE client txn was not displaced"
    );
}

// ── Client timeout (Timer B) ────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn timer_b_emits_timeout_event() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    // An IN-DIALOG re-INVITE (To-tag present) keeps the 32 s Timer B.
    stack
        .txn
        .send_request(outbound_reinvite("z9hG4bK-tb"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();

    // Timer B fires at 64·T1 = 32 s with no final response.
    elapse_ms(35_000).await;

    // The Timeout event carries the method, the destination it was sent to, and
    // the discriminator (Timer B → Response) for the per-peer failure metric.
    let timeout = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { method, destination, kind, .. } => {
            Some((method, destination, kind))
        }
        _ => None,
    });
    let (method, destination, kind) = timeout.expect("Timer B emits a Timeout event");
    assert_eq!(method, Some("INVITE".to_string()));
    assert_eq!(destination, Some(addr(PEER)), "Timeout forwards the txn destination");
    assert_eq!(
        kind,
        sip_txn::TimeoutKind::Response,
        "an in-dialog re-INVITE Timer B is a Response timeout"
    );
    assert_eq!(active(&stack), 0, "timed-out txn is removed");
}

/// A RINGING initial (out-of-dialog) INVITE must NOT expire at the 32 s Timer-B
/// mark — a callee may legitimately ring past it, and the upper layer's
/// no-answer timer (≤180 s) owns that deadline (a clean CANCEL→487). We keep
/// only a hard backstop at [`INVITE_INITIAL_TIMEOUT`] = 158 s (below the 180 s
/// Timer-C mark), so the no-answer always fires first and the 3-minute timer
/// never beats us. Ringing is what the first provisional establishes: the
/// no-response twin is `unanswered_initial_invite_times_out_on_timer_b`, and
/// the raised-config twin is
/// `configured_invite_bound_moves_the_initial_invite_expiry`.
#[tokio::test(start_paused = true)]
async fn initial_invite_outlives_the_no_answer_window() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-init"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    // The callee rings: the first provisional is what opens the ring window
    // the 158 s bound owns (§17.1.1.2 scopes Timer B to Calling).
    stack
        .inject(&response_bytes(
            180,
            "Ringing",
            "INVITE",
            "z9hG4bK-init",
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    stack.drain_events();

    // No Timeout at 35 s — still ringing.
    elapse_ms(35_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "initial INVITE must not expire inside the ring window"
    );
    assert_eq!(active(&stack), 1, "still live, ringing");

    // The 158 s backstop eventually fires (total elapsed ~165 s, below 180 s).
    elapse_ms(130_000).await;
    let kind = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { kind, .. } => Some(kind),
        _ => None,
    });
    assert_eq!(
        kind,
        Some(sip_txn::TimeoutKind::Transaction),
        "the initial-INVITE backstop fires below the 3-minute mark and is a Transaction timeout"
    );
    assert_eq!(active(&stack), 0);
}

/// An in-dialog re-INVITE that has drawn a provisional has left Calling, the
/// only state Timer B governs (RFC 3261 §17.1.1.2): it does NOT expire at the
/// 32 s mark — the peer that answered 1xx is not a dead hop — and ends on the
/// same pre-final backstop as a ringing initial INVITE, reported as a
/// `Transaction` timeout. Its no-response twin is `timer_b_emits_timeout_event`.
#[tokio::test(start_paused = true)]
async fn a_reinvite_that_drew_a_provisional_outlives_timer_b() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .txn
        .send_request(outbound_reinvite("z9hG4bK-reinv-1xx"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(
            183,
            "Session Progress",
            "INVITE",
            "z9hG4bK-reinv-1xx",
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    stack.drain_events();

    // No Timeout at 35 s — the renegotiation is still in progress.
    elapse_ms(35_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "a re-INVITE in Proceeding is not on Timer B"
    );
    assert_eq!(active(&stack), 1, "still live, awaiting its final");

    // The backstop fires (total elapsed ~165 s), as a Transaction timeout.
    elapse_ms(130_000).await;
    let kind = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { kind, .. } => Some(kind),
        _ => None,
    });
    assert_eq!(
        kind,
        Some(sip_txn::TimeoutKind::Transaction),
        "the INVITE backstop answers a silent renegotiation, not the peer-failure timer"
    );
    assert_eq!(active(&stack), 0);
}

/// An initial INVITE that draws NO response of any kind — not even the `100
/// Trying` a UAS sends before it does anything else (§17.2.1) — is an
/// unanswered hop, not a ringing callee: RFC 3261 §17.1.1.2 scopes Timer B to
/// the Calling state, so it gives up at 64·T1 and reports a `Response` timeout.
/// The long initial bound never covers it — nothing is ringing to protect, and
/// leaving the caller on a dead next hop until the app's no-answer deadline
/// hides an unreachable destination behind "no user responding".
#[tokio::test(start_paused = true)]
async fn unanswered_initial_invite_times_out_on_timer_b() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-blackhole"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();

    // Still Calling at 31 s: the Timer-A ladder has run, nothing came back.
    elapse_ms(31_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "Timer B has not reached 64·T1 yet"
    );
    assert_eq!(active(&stack), 1);

    // 64·T1 = 32 s.
    elapse_ms(2_000).await;
    let kind = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { kind, .. } => Some(kind),
        _ => None,
    });
    assert_eq!(
        kind,
        Some(sip_txn::TimeoutKind::Response),
        "an INVITE nothing answered times out on Timer B, not the 158 s bound"
    );
    assert_eq!(active(&stack), 0);
}

/// The out-of-dialog INVITE bound is a deployment tunable
/// (`TransactionConfig::invite_initial_timeout_ms`): raised to 300 s, a RINGING
/// initial INVITE survives the default 158 s mark and gives up only at the
/// configured bound (a `Transaction` timeout) — while an in-dialog re-INVITE
/// under the SAME config keeps the 32 s Timer B failure detection (`Response`).
#[tokio::test(start_paused = true)]
async fn configured_invite_bound_moves_the_initial_invite_expiry() {
    let mut stack = Stack::build_with_config(
        TRANSIT,
        64,
        sip_txn::TransactionConfig {
            udp_queue_max: 64,
            id_gen: std::sync::Arc::new(sip_txn::IdGen::seeded(0xC0FFEE)),
            invite_initial_timeout_ms: 300_000,
            ..Default::default()
        },
    )
    .await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-cfg300"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .inject(&response_bytes(
            180,
            "Ringing",
            "INVITE",
            "z9hG4bK-cfg300",
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    stack.drain_events();

    // Past the DEFAULT 158 s bound (~165 s): still ringing, no Timeout.
    elapse_ms(165_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "a 300 s-configured initial INVITE must not expire at the default 158 s mark"
    );
    assert_eq!(active(&stack), 1, "still live, ringing");

    // The configured 300 s bound fires (total elapsed ~305 s).
    elapse_ms(140_000).await;
    let kind = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { kind, .. } => Some(kind),
        _ => None,
    });
    assert_eq!(
        kind,
        Some(sip_txn::TimeoutKind::Transaction),
        "the configured bound fires as a Transaction timeout"
    );
    assert_eq!(active(&stack), 0);

    // An in-dialog re-INVITE (To-tag present) under the SAME config keeps the
    // 32 s Timer B and reports a Response timeout.
    stack.drain_peer();
    stack
        .txn
        .send_request(outbound_reinvite("z9hG4bK-cfg300-reinv"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(35_000).await;
    let kind = stack.drain_events().into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { kind, .. } => Some(kind),
        _ => None,
    });
    assert_eq!(
        kind,
        Some(sip_txn::TimeoutKind::Response),
        "an in-dialog re-INVITE still times out at Timer B with a Response timeout"
    );
    assert_eq!(active(&stack), 0);
}

fn first_response_bound_config(ms: u64) -> sip_txn::TransactionConfig {
    sip_txn::TransactionConfig {
        udp_queue_max: 64,
        id_gen: std::sync::Arc::new(sip_txn::IdGen::seeded(0xC0FFEE)),
        invite_first_response_timeout_ms: ms,
        ..Default::default()
    }
}

fn timeout_kind(events: Vec<TransactionEvent>) -> Option<sip_txn::TimeoutKind> {
    events.into_iter().find_map(|e| match e {
        TransactionEvent::Timeout { kind, .. } => Some(kind),
        _ => None,
    })
}

/// The initial INVITE's first-response bound is a deployment tunable
/// (`TransactionConfig::invite_first_response_timeout_ms`, default Timer B):
/// tightened to 5 s, an initial INVITE nothing answers gives up at 5 s with a
/// `Response` timeout — the hop is dead — after exactly the three Timer A
/// re-sends the bound buys (0.5 / 1.5 / 3.5 s): the ladder is armed under the
/// same bound, so no rung lands past the give-up.
#[tokio::test(start_paused = true)]
async fn tightened_first_response_bound_fails_an_unanswered_initial_invite_early() {
    let mut stack = Stack::build_with_config(TRANSIT, 64, first_response_bound_config(5_000)).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-fr5"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();

    // Still Calling at 4 s: the three rungs the bound buys have all fired.
    elapse_ms(4_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "the 5 s bound has not elapsed yet"
    );
    assert_eq!(active(&stack), 1);
    assert_eq!(
        count_requests(&stack.drain_peer(), "INVITE"),
        4,
        "original send + rungs at 0.5 / 1.5 / 3.5 s"
    );

    // The bound fires; no fourth rung (7.5 s) was ever armed.
    elapse_ms(1_100).await;
    assert_eq!(
        timeout_kind(stack.drain_events()),
        Some(sip_txn::TimeoutKind::Response),
        "an INVITE nothing answered times out on the first-response bound as a dead hop"
    );
    assert_eq!(active(&stack), 0);
    assert_eq!(count_requests(&stack.drain_peer(), "INVITE"), 0, "no rung past the give-up");
    assert_eq!(
        stack.txn.metrics().retransmit_rows(),
        vec![RetransmitRow { ladder: "invite-client", method: "INVITE", code: None, count: 3 }],
    );
}

/// The tightened first-response bound reads ONLY on an initial INVITE: under
/// the same 5 s config an in-dialog INVITE (To-tag present) keeps the 32 s
/// Timer B and a non-INVITE keeps Timer F — a re-INVITE into a live call or a
/// BYE is a different risk, and a transient blip must not tear a call down.
#[tokio::test(start_paused = true)]
async fn tightened_first_response_bound_leaves_in_dialog_and_non_invite_on_64_t1() {
    let mut stack = Stack::build_with_config(TRANSIT, 64, first_response_bound_config(5_000)).await;
    stack
        .txn
        .send_request(outbound_reinvite("z9hG4bK-fr5-reinv"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    stack
        .txn
        .send_request(outbound_request("BYE", "z9hG4bK-fr5-bye"), addr(PEER), TxnKind::NonInvite)
        .await
        .unwrap();

    // Well past the 5 s bound: both still live.
    elapse_ms(31_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "neither an in-dialog INVITE nor a non-INVITE reads the first-response bound"
    );
    assert_eq!(active(&stack), 2);

    // 64·T1 = 32 s: Timer B and Timer F both fire, as Response timeouts.
    elapse_ms(2_000).await;
    let kinds: Vec<_> = stack
        .drain_events()
        .into_iter()
        .filter_map(|e| match e {
            TransactionEvent::Timeout { method, kind, .. } => Some((method, kind)),
            _ => None,
        })
        .collect();
    assert_eq!(kinds.len(), 2, "one Timeout per transaction: {kinds:?}");
    assert!(
        kinds.iter().all(|(_, k)| *k == sip_txn::TimeoutKind::Response),
        "Timer B / Timer F are Response timeouts: {kinds:?}"
    );
    assert_eq!(
        kinds.iter().filter(|(m, _)| m.as_deref() == Some("INVITE")).count(),
        1,
        "one is the re-INVITE's Timer B, the other the BYE's Timer F: {kinds:?}"
    );
    assert_eq!(active(&stack), 0);
}

/// A ringing callee is unaffected by the tightened first-response bound: the
/// FIRST provisional swaps in the long INVITE bound
/// (`invite_initial_timeout_ms`, default 158 s), so an initial INVITE that
/// drew a 100 / 180 at 2 s does not expire at 5 s and gives up only at the
/// long bound, as a `Transaction` timeout — the callee answered, then went
/// silent.
#[tokio::test(start_paused = true)]
async fn a_provisional_before_the_first_response_bound_swaps_in_the_long_bound() {
    let mut stack = Stack::build_with_config(TRANSIT, 64, first_response_bound_config(5_000)).await;
    stack
        .txn
        .send_request(outbound_request("INVITE", "z9hG4bK-fr5-ring"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(2_000).await;
    stack
        .inject(&response_bytes(
            180,
            "Ringing",
            "INVITE",
            "z9hG4bK-fr5-ring",
            "handle-shape-test",
            true,
        ))
        .await;
    elapse_ms(20).await;
    stack.drain_events();
    stack.drain_peer();

    // Past the 5 s bound (~7 s): still ringing, no Timeout, no more rungs.
    elapse_ms(5_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "a ringing initial INVITE is not on the first-response bound"
    );
    assert_eq!(active(&stack), 1, "still live, ringing");
    assert_eq!(
        count_requests(&stack.drain_peer(), "INVITE"),
        0,
        "the provisional stopped the ladder"
    );

    // The long bound fires (measured from the original send: ~158 s).
    elapse_ms(153_000).await;
    assert_eq!(
        timeout_kind(stack.drain_events()),
        Some(sip_txn::TimeoutKind::Transaction),
        "the long INVITE bound answers a callee that rang and went silent"
    );
    assert_eq!(active(&stack), 0);
}

// ── CANCEL (server INVITE) ──────────────────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn cancel_sends_200_and_487_and_emits_cancelled() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-cxl";

    stack.inject(&inbound_request("INVITE", branch, "cxl-call", None)).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "100 Trying for INVITE");
    assert!(has_message_request(&stack.drain_events(), "INVITE"));

    stack.inject(&inbound_request("CANCEL", branch, "cxl-call", None)).await;
    elapse_ms(60).await;

    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1, "200 OK to the CANCEL");
    assert_eq!(count_responses(&out, 487), 1, "487 Request Terminated on the INVITE");
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "a Cancelled event is emitted"
    );
}

/// A CANCEL matching no active INVITE server txn has NO effect here — never a
/// 200 + a spurious Cancelled that tears a call down — and is handed up as a
/// `Message` unanswered: the TU owns the RFC 3261 §9.2 481, or the re-offer
/// against an INVITE it rebuilds for a call taken over from a peer (ADR-0014).
#[tokio::test(start_paused = true)]
async fn unmatched_cancel_is_handed_up_unanswered() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    stack.inject(&inbound_request("CANCEL", "z9hG4bK-stray", "stray-call", None)).await;
    elapse_ms(60).await;

    assert!(stack.drain_peer().is_empty(), "the layer answers an unmatched CANCEL with nothing");
    let events = stack.drain_events();
    assert!(
        !events.iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "an unmatched CANCEL must not surface a Cancelled"
    );
    assert!(has_message_request(&events, "CANCEL"), "the CANCEL itself reaches the consumer");
    assert_eq!(active(&stack), 0, "and builds no transaction");
}

/// A CANCEL arriving after the INVITE was answered (200 raced the CANCEL) finds
/// the server txn held past its final (RFC 6026 §7.1), so the layer answers it
/// 200 itself (RFC 3261 §9.2): no 487 that would tear the established call
/// down, nothing handed up, and the INVITE txn is untouched.
#[tokio::test(start_paused = true)]
async fn cancel_after_answer_does_not_tear_down_the_call() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-late-cxl";
    let call_id = "late-call";

    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();
    let resp = parse_response(&response_bytes(200, "OK", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    stack.inject(&inbound_request("CANCEL", branch, call_id, None)).await;
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 1, "the late CANCEL is answered 200: {out:?}");
    assert_eq!(count_responses(&out, 487), 0, "and draws no 487: {out:?}");
    let events = stack.drain_events();
    assert!(
        !events.iter().any(|e| matches!(e, TransactionEvent::Cancelled { .. })),
        "no Cancelled for a CANCEL after answer"
    );
    assert!(!has_message_request(&events, "CANCEL"), "the layer answered it; nothing handed up");
    assert_eq!(active(&stack), 1, "the answered INVITE txn stays for its timer");
}

// ── ACK absorption (server INVITE) ──────────────────────────────────────────

async fn invite_then_final(stack: &mut Stack, branch: &str, call_id: &str, status: u16) {
    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    let _ = stack.drain_events();
    assert_eq!(active(stack), 1);

    // The application sends the final response through its server txn.
    let resp = parse_response(&response_bytes(status, "Final", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();
    assert_eq!(active(stack), 1, "completed txn pinned for Timer H");
}

/// RFC 3261 §17.2.1: the ACK for a non-2xx final moves the server txn to
/// Confirmed, where it stays for Timer I absorbing retransmitted ACKs, and
/// only then leaves the map.
#[tokio::test(start_paused = true)]
async fn ack_for_non_2xx_is_absorbed() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-ackn";
    invite_then_final(&mut stack, branch, "ackn-call", 480).await;

    stack.inject(&inbound_request("ACK", branch, "ackn-call", Some("peer-tag"))).await;
    elapse_ms(60).await;

    assert_eq!(active(&stack), 1, "ACK for non-2xx holds the txn in Confirmed for Timer I");
    assert!(
        !has_message_request(&stack.drain_events(), "ACK"),
        "ACK for non-2xx must NOT surface to the app"
    );

    // A retransmitted ACK inside Timer I is absorbed the same way.
    stack.inject(&inbound_request("ACK", branch, "ackn-call", Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(active(&stack), 1, "still Confirmed");
    assert!(
        !has_message_request(&stack.drain_events(), "ACK"),
        "a retransmitted ACK in Confirmed is absorbed silently"
    );
    assert!(stack.drain_peer().is_empty(), "nothing leaves on an ACK");

    elapse_ms(TIMER_I).await;
    assert_eq!(active(&stack), 0, "Timer I terminates the txn");
}

#[tokio::test(start_paused = true)]
async fn ack_for_2xx_passes_through() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-ack2";
    invite_then_final(&mut stack, branch, "ack2-call", 200).await;

    stack.inject(&inbound_request("ACK", branch, "ack2-call", Some("peer-tag"))).await;
    elapse_ms(60).await;

    assert_eq!(active(&stack), 0, "ACK for 2xx terminates the server txn");
    assert!(
        has_message_request(&stack.drain_events(), "ACK"),
        "ACK for 2xx is delivered to the app"
    );
}

/// A second final on an already-Completed server txn (a 200 racing the
/// autonomous 487, or a duplicate relayed final) must be DROPPED — not put a
/// second final with a different To-tag on the wire and flip the ACK classifier.
#[tokio::test(start_paused = true)]
async fn duplicate_final_on_completed_server_txn_is_dropped() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-dupfinal";
    let call_id = "dupfinal-call";

    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    let _ = stack.drain_peer();

    // First final: 487 → Completed, classifier = non-2xx.
    let first =
        parse_response(&response_bytes(487, "Request Terminated", "INVITE", branch, call_id, true));
    stack.txn.send_response(first, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 487), 1);

    // Second, conflicting final: 200 → must be dropped (no wire, no state flip).
    let second = parse_response(&response_bytes(200, "OK", "INVITE", branch, call_id, true));
    stack.txn.send_response(second, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    let out = stack.drain_peer();
    assert_eq!(count_responses(&out, 200), 0, "conflicting final dropped");
    assert_eq!(count_responses(&out, 487), 0, "no re-send");

    // The ACK for 487 is still absorbed as non-2xx (classifier intact, not 200).
    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert!(
        !has_message_request(&stack.drain_events(), "ACK"),
        "ACK for 487 absorbed, not surfaced as a 2xx ACK"
    );
    assert_eq!(active(&stack), 1, "Confirmed for Timer I");

    // A final offered after the ACK (a teardown timer answering a cancelled
    // call) is dropped just the same: the branch carries its final.
    let third =
        parse_response(&response_bytes(480, "Unavailable", "INVITE", branch, call_id, true));
    stack.txn.send_response(third, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 480), 0, "a final in Confirmed is dropped");
    assert_eq!(stack.txn.metrics().server_final_unseen_branch(), 0, "the branch is held");

    elapse_ms(TIMER_I).await;
    assert_eq!(active(&stack), 0, "Timer I terminates the txn");

    // And once the transaction is gone, a non-2xx final on the branch is
    // dropped and counted rather than put on the wire raw.
    let late = parse_response(&response_bytes(480, "Unavailable", "INVITE", branch, call_id, true));
    stack.txn.send_response(late, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 480), 0, "no final on a branch without a txn");
    assert_eq!(stack.txn.metrics().server_final_unseen_branch(), 1, "counted as dropped");
}

// ── Timer G: server INVITE non-2xx final retransmit (§17.2.1) ───────────────

/// RFC 3261 §17.2.1: an INVITE server txn that answered NON-2xx MUST actively
/// retransmit the final (Timer G: T1, then ×2 capped at T2) until the ACK or
/// Timer H. The auto-100 we sent already silenced the UAC's INVITE retransmit,
/// so the passive replay-on-request-retransmit path never fires — without Timer G
/// a single dropped reject wedges the caller for the full 32 s.
#[tokio::test(start_paused = true)]
async fn server_invite_non_2xx_final_retransmits_on_timer_g() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-timerg";
    let call_id = "timerg-call";

    stack.inject(&inbound_request("INVITE", branch, call_id, None)).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "100 Trying");
    let _ = stack.drain_events();

    // App rejects with 603 → Completed, sent ONCE, Timer G armed at T1.
    let resp = parse_response(&response_bytes(603, "Decline", "INVITE", branch, call_id, true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 603), 1, "603 sent once");
    assert_eq!(active(&stack), 1, "completed txn pinned (Timer G/H)");

    // The caller never ACKs (the 603 was dropped on the wire). Over the next ~8 s
    // Timer G fires at 500 / 1500 / 3500 / 7500 ms (interval 500→1000→2000→4000,
    // capped at T2) = FOUR retransmits — any one heals a per-datagram loss.
    elapse_ms(8_000).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 603),
        4,
        "Timer G retransmits the non-2xx final (×2 cadence capped at T2)"
    );
    assert_eq!(
        stack.txn.metrics().server_final_retransmits(),
        4,
        "the counter tracks the Timer-G retransmits"
    );
    assert_eq!(
        stack.txn.metrics().retransmit_rows(),
        vec![RetransmitRow {
            ladder: "invite-server-final",
            method: "INVITE",
            code: Some(603),
            count: 4
        }],
        "each Timer G rung is counted under the final's CSeq method and status",
    );
    assert_eq!(active(&stack), 1, "still Completed (unACKed), bounded by Timer H");

    // The ACK finally lands → Timer G cancelled, txn Confirmed for Timer I,
    // silence after.
    stack.inject(&inbound_request("ACK", branch, call_id, Some("peer-tag"))).await;
    elapse_ms(60).await;
    assert_eq!(active(&stack), 1, "the ACK holds the txn in Confirmed (Timer G cancelled)");
    elapse_ms(TIMER_I - 100).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 603),
        0,
        "no retransmit after the ACK — Timer G was cancelled on entering Confirmed"
    );
    assert_eq!(active(&stack), 1, "still Confirmed inside Timer I");
    elapse_ms(200).await;
    assert_eq!(active(&stack), 0, "Timer I terminates the txn");
    assert!(stack.drain_peer().is_empty(), "silence past Timer I");
}

/// A 2xx final on an INVITE server txn is EXEMPT from Timer G (§17.2.1: the
/// server txn is done on a 2xx; the TU owns §13.3.1.4 2xx retransmission). Timer G
/// firing here would put a duplicate 200 on the wire that the TU's own watchdog
/// already covers, so the server txn must stay silent until its ACK.
#[tokio::test(start_paused = true)]
async fn server_invite_2xx_final_is_not_timer_g_retransmitted() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-noG2xx";
    let call_id = "noG2xx-call";
    invite_then_final(&mut stack, branch, call_id, 200).await;

    // Well past T1/T2 with no ACK — the server txn must NOT retransmit the 200.
    elapse_ms(8_000).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 0, "no Timer-G resend for a 2xx");
    assert_eq!(stack.txn.metrics().server_final_retransmits(), 0, "counter untouched by 2xx");
    assert_eq!(active(&stack), 1, "still held in Completed for Timer H / the 2xx ACK");
}

// ── Auto-ACK for non-2xx (client INVITE) ────────────────────────────────────

#[tokio::test(start_paused = true)]
async fn client_auto_acks_non_2xx_final() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-autoack";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer(); // the initial INVITE

    // Peer answers 480 — the transaction layer must ACK it hop-by-hop.
    stack
        .inject(&response_bytes(
            480,
            "Temporarily Unavailable",
            "INVITE",
            branch,
            "autoack-call",
            true,
        ))
        .await;
    elapse_ms(60).await;

    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "auto-ACK for non-2xx");
    let events = stack.drain_events();
    assert!(
        events.iter().any(|e| matches!(e,
            TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Response(r) if r.status() == 480))),
        "the 480 still surfaces to the app"
    );
    // Held in Completed for Timer D (re-ACK/absorb window), not deleted on the spot.
    assert_eq!(active(&stack), 1, "client txn held in Completed for Timer D");
}

/// After ACKing a non-2xx INVITE final the client txn stays in Completed for
/// Timer D (32 s): a retransmitted final (our ACK was lost) is RE-ACKed and
/// absorbed — not re-surfaced — then the txn terminates at Timer D.
#[tokio::test(start_paused = true)]
async fn non_2xx_invite_final_absorbs_retransmits_for_timer_d() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-timerd";
    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(60).await;
    let _ = stack.drain_peer();

    // First 486 → auto-ACK, surfaces once, txn held for Timer D.
    stack.inject(&response_bytes(486, "Busy Here", "INVITE", branch, "timerd-call", true)).await;
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "auto-ACK for the 486");
    assert_eq!(
        stack
            .drain_events()
            .iter()
            .filter(|e| matches!(e, TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Response(r) if r.status() == 486)))
            .count(),
        1,
        "the 486 surfaces exactly once"
    );
    assert_eq!(active(&stack), 1, "held in Completed for Timer D");

    // Retransmitted 486 (first ACK lost) → RE-ACK, absorbed (no second Message).
    stack.inject(&response_bytes(486, "Busy Here", "INVITE", branch, "timerd-call", true)).await;
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "retransmitted 486 re-ACKed");
    assert!(stack.drain_events().is_empty(), "retransmitted final must not re-surface");

    // Timer D (32 s) fires → the client txn terminates.
    elapse_ms(33_000).await;
    assert_eq!(active(&stack), 0, "Timer D cleaned up the client txn");
}

// ── Duplicate request → cached-response retransmit ──────────────────────────

#[tokio::test(start_paused = true)]
async fn duplicate_request_retransmits_cached_response() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-dup";

    stack.inject(&inbound_request("OPTIONS", branch, "dup-call", None)).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "OPTIONS"));

    // App answers 200 — cached on the server txn.
    let resp = parse_response(&response_bytes(200, "OK", "OPTIONS", branch, "dup-call", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);

    // The retransmitted OPTIONS must replay the cached 200, not re-surface.
    stack.inject(&inbound_request("OPTIONS", branch, "dup-call", None)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 200),
        1,
        "cached 200 retransmitted for the duplicate"
    );
    assert!(stack.drain_events().is_empty(), "duplicate request must not surface a second time");
    // A cached FINAL replayed is a `trigger` repeat too, under its own status.
    assert_eq!(
        stack.txn.metrics().retransmit_rows(),
        vec![RetransmitRow { ladder: "trigger", method: "OPTIONS", code: Some(200), count: 1 }],
    );
}

// ── cancel_txns_for_call spares server txns (Timer-J absorption) ────────────

/// Tearing a call down (`cancel_txns_for_call`) must cancel only CLIENT txns —
/// a Completed SERVER txn keeps its Timer-J retransmit-absorption window so a
/// BYE retransmit after teardown replays the cached 200 instead of building a
/// fresh txn that 481s upstream (the unexpected-481-on-BYE wire signature).
#[tokio::test(start_paused = true)]
async fn cancel_txns_for_call_spares_server_timer_j_absorption() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let cr = "w0|bye-cr";
    let branch = "z9hG4bK-bye";

    // Inbound BYE attributed to the call (R-URI callref) → server txn.
    stack.inject(&inbound_with_callref("BYE", branch, "cid-bye", cr)).await;
    elapse_ms(60).await;
    assert!(has_message_request(&stack.drain_events(), "BYE"));

    // App answers 200 → server txn Completed, 200 cached, Timer J armed.
    let resp = parse_response(&response_bytes(200, "OK", "BYE", branch, "cid-bye", true));
    stack.txn.send_response(resp, addr(PEER)).await.unwrap();
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 200), 1);

    // Call torn down — the server txn must SURVIVE this.
    stack.txn.cancel_txns_for_call(cr).await.unwrap();

    // Retransmitted BYE → cached 200 replayed, NOT re-surfaced to the app.
    stack.inject(&inbound_with_callref("BYE", branch, "cid-bye", cr)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 200),
        1,
        "cached 200 replayed for the BYE retransmit"
    );
    assert!(
        stack.drain_events().is_empty(),
        "retransmitted BYE must not re-surface (no orphan 481 path)"
    );
}

/// A one-shot Timeout must be DEFERRED, never dropped, when the events queue is
/// full — the post-failover storm saturates it exactly when reclaimed calls time
/// out, and a lost Timeout strands the leg until the 1 h GlobalDuration backstop.
#[tokio::test(start_paused = true)]
async fn timeout_survives_a_full_event_queue() {
    // udp_queue_max 16 → event-queue capacity max(64, 16*4) = 64; generous recv
    // queue so all injected OPTIONS reach the owner and fill the event queue.
    let mut stack = Stack::build(TRANSIT, 16, 1024).await;
    // An in-dialog client txn whose Timer B (32 s) will fire.
    stack
        .txn
        .send_request(outbound_reinvite("z9hG4bK-tofull"), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();

    // Saturate the events queue with undrained inbound OPTIONS (lossy Messages).
    for i in 0..70 {
        stack
            .inject(&inbound_request(
                "OPTIONS",
                &format!("z9hG4bK-q{i}"),
                &format!("cid-q{i}"),
                None,
            ))
            .await;
    }
    elapse_ms(60).await;

    // Timer B fires (32 s) into the full queue → the Timeout must DEFER, not drop.
    elapse_ms(33_000).await;
    assert!(
        !stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "the full-queue instant must have deferred the Timeout, not squeezed it in"
    );
    // Capacity returned (we just drained) → the retry tick redelivers it.
    elapse_ms(150).await;
    assert!(
        stack.drain_events().iter().any(|e| matches!(e, TransactionEvent::Timeout { .. })),
        "deferred Timeout redelivered once queue capacity returned"
    );
}

/// The auto-sent 100 Trying is cached, so a retransmitted INVITE replays it (RFC
/// 3261 §17.2.1) instead of being absorbed silently — the 100 already silenced
/// the UAC's own retransmit timer, so a black hole here fails the call.
#[tokio::test(start_paused = true)]
async fn retransmitted_invite_replays_the_cached_100() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-100cache";
    stack.inject(&inbound_request("INVITE", branch, "cid-100", None)).await;
    elapse_ms(60).await;
    assert_eq!(count_responses(&stack.drain_peer(), 100), 1, "100 Trying for the INVITE");
    let _ = stack.drain_events();

    // Retransmitted INVITE (same branch) before any final → replays the cached 100.
    stack.inject(&inbound_request("INVITE", branch, "cid-100", None)).await;
    elapse_ms(60).await;
    assert_eq!(
        count_responses(&stack.drain_peer(), 100),
        1,
        "cached 100 replayed for the INVITE retransmit"
    );
    assert!(stack.drain_events().is_empty(), "retransmitted INVITE must not re-surface to the app");
    // The replay is a repeat the peer provoked, not a ladder rung: counted
    // as `trigger`, under the request's method and the cached status.
    assert_eq!(
        stack.txn.metrics().retransmit_rows(),
        vec![RetransmitRow { ladder: "trigger", method: "INVITE", code: Some(100), count: 1 }],
    );
    assert_eq!(stack.txn.metrics().triggered_retransmits(), 1);
}

// ── #9: call_ref → txn-count index (acting-backup self-release gate) ─────────

/// An inbound in-dialog request whose Request-URI carries the `callref` param the
/// B2BUA's Contact stamps — the key the txn layer attributes the server txn to
/// (ADR-0014 self-release counting). `extract_ruri_call_ref` percent-decodes it,
/// so a plain value round-trips unchanged.
fn inbound_with_callref(method: &str, branch: &str, call_id: &str, call_ref: &str) -> Vec<u8> {
    format!(
        "{method} sip:b2bua@127.0.0.1:5070;callref={call_ref} SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5555;branch={branch}\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:caller@10.0.0.1:5555>;tag=caller-tag\r\n\
         To: <sip:b2bua@127.0.0.1:5070>;tag=dlg-tag\r\n\
         Call-ID: {call_id}\r\n\
         CSeq: 1 {method}\r\n\
         Content-Length: 0\r\n\r\n"
    )
    .into_bytes()
}

fn drained_quiesced(stack: &mut Stack, call_ref: &str) -> bool {
    stack
        .drain_events()
        .iter()
        .any(|e| matches!(e, TransactionEvent::CallQuiesced { call_ref: cr } if cr == call_ref))
}

/// The `call_ref → txn-count` index stays in lockstep with the `txns` map, so the
/// acting-backup self-release gate is EXACT: two concurrent in-dialog txns for one
/// call count as 2, and the armed watch fires `CallQuiesced` only once BOTH clear
/// — never after just the first. This is the txn-layer guarantee the B2BUA leans
/// on to avoid shedding a takeover copy while a transaction is still in flight.
#[tokio::test(start_paused = true)]
async fn self_release_fires_only_after_the_last_call_txn_clears() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let cr = "w0z-cid|caller-tag";

    // Two in-dialog requests for the SAME call, distinct branches → 2 server txns.
    stack.inject(&inbound_with_callref("OPTIONS", "z9hG4bK-r1", "cid-sr", cr)).await;
    stack.inject(&inbound_with_callref("OPTIONS", "z9hG4bK-r2", "cid-sr", cr)).await;
    elapse_ms(60).await;
    let _ = stack.drain_events();
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await.unwrap(), 2, "both txns counted");
    assert_eq!(
        stack.txn.active_txn_count_for_call("w0z-other|t").await.unwrap(),
        0,
        "a different call_ref is isolated in the index"
    );

    // Arm the watch while txns are live → NO immediate CallQuiesced.
    stack.txn.watch_self_release(cr).await.unwrap();
    assert!(!drained_quiesced(&mut stack, cr), "must not fire while txns are in flight");

    // Clear the FIRST txn (200 → non-INVITE server Timer J eviction). Count → 1.
    let r1 = parse_response(&response_bytes(200, "OK", "OPTIONS", "z9hG4bK-r1", "cid-sr", true));
    stack.txn.send_response(r1, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await; // past TIMER_J (64*T1 = 32 s)
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await.unwrap(), 1, "one txn cleared");
    assert!(!drained_quiesced(&mut stack, cr), "one txn still live → no self-release");

    // Clear the SECOND: the last `delete_txn` for the call fires CallQuiesced.
    let r2 = parse_response(&response_bytes(200, "OK", "OPTIONS", "z9hG4bK-r2", "cid-sr", true));
    stack.txn.send_response(r2, addr(PEER)).await.unwrap();
    elapse_ms(33_000).await;
    assert_eq!(stack.txn.active_txn_count_for_call(cr).await.unwrap(), 0, "index drained to 0");
    assert!(drained_quiesced(&mut stack, cr), "last txn cleared → CallQuiesced");
}

/// A FULL events queue must DEFER `CallQuiesced`, never destroy it. It is the
/// only self-release trigger a takeover copy gets, and the queue saturates
/// exactly when takeover copies exist (the post-failover storm) — the old
/// drop-newest `emit` stranded the copy double-serving until its 1 h
/// `GlobalDuration` backstop. The watch stays armed until the send lands, and
/// the `QuiescedRetry` tick re-offers once the consumer drains the backlog.
#[tokio::test(start_paused = true)]
async fn call_quiesced_survives_a_full_event_queue() {
    // udp_queue_max 16 → event-queue capacity max(64, 16*4) = 64.
    let mut stack = Stack::build(TRANSIT, 16, 64).await;
    let cr = "w0z-full|caller-tag";

    // Saturate the bounded events queue: 70 undrained inbound OPTIONS (each
    // emits one Message event; 65+ are drop-newest discarded — that class is
    // legitimately lossy, CallQuiesced is not).
    for i in 0..70 {
        stack
            .inject(&inbound_request(
                "OPTIONS",
                &format!("z9hG4bK-fq{i}"),
                &format!("cid-fq{i}"),
                None,
            ))
            .await;
    }
    elapse_ms(60).await;

    // The watch's immediate-fire path (no txns for `cr`) hits the full queue.
    stack.txn.watch_self_release(cr).await.unwrap();

    // The backlog the consumer now drains does NOT contain the notice…
    assert!(
        !drained_quiesced(&mut stack, cr),
        "the full-queue instant must not have squeezed the notice in"
    );
    // …but the deferred delivery lands on the next retry tick.
    elapse_ms(150).await;
    assert!(
        drained_quiesced(&mut stack, cr),
        "deferred CallQuiesced is re-delivered once queue capacity returns"
    );
}

/// A stray 100 arriving AFTER a non-2xx final must not downgrade the Completed
/// client txn back to Proceeding: the Timer-D absorption window stays intact,
/// so a retransmitted final is re-ACKed and absorbed — not re-surfaced to the
/// TU as a duplicate first final.
#[tokio::test(start_paused = true)]
async fn late_100_does_not_reopen_a_completed_invite_txn() {
    let mut stack = Stack::build(TRANSIT, 64, 64).await;
    let branch = "z9hG4bK-late-100";

    stack
        .txn
        .send_request(outbound_request("INVITE", branch), addr(PEER), TxnKind::Invite)
        .await
        .unwrap();
    elapse_ms(60).await;
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "first final auto-ACKed");
    let _ = stack.drain_events();

    // Stray/late 100, then the UAS retransmits its 486 (as if our ACK was lost).
    stack
        .inject(&response_bytes(100, "Trying", "INVITE", branch, "handle-shape-test", false))
        .await;
    elapse_ms(60).await;
    stack
        .inject(&response_bytes(486, "Busy Here", "INVITE", branch, "handle-shape-test", true))
        .await;
    elapse_ms(60).await;
    assert_eq!(count_requests(&stack.drain_peer(), "ACK"), 1, "retransmitted final re-ACKed");
    assert!(
        !stack.drain_events().iter().any(|e| matches!(
            e,
            TransactionEvent::Message { message, .. }
                if matches!(message.as_ref(), SipMessage::Response(r) if r.status() == 486)
        )),
        "the retransmitted 486 is absorbed, not re-surfaced"
    );
}
