//! **A rerouted attempt's rejection rides the caller leg's PRIMARY tag** — the
//! `/call/failure` b-leg failover path, transparent 18x.
//!
//! Attempt 1 rings and mints the caller's early dialog; it fails, and
//! `/call/failure` opens a SECOND b-leg toward another callee. The caller-facing
//! final, whichever attempt and whichever of that attempt's forks produced it,
//! rides the tag attempt 1 minted — the same split
//! [`forked_callee_rejection_tag`] pins within one leg, holding across the leg
//! swap.
//!
//! **Evidence level, stated honestly.** The corpus attests the ADJACENT shape,
//! not this one: `capture_c400eba5` group 1 is a serial hunt on the CALLEE side
//! (`6.1.2.1` then `6.1.2.2`, two fork tags on ONE upstream transaction) and is
//! H2 — that is the shape [`forked_callee_rejection_tag`] covers. A reroute that
//! is a fresh downstream TRANSACTION is the adjacent one, and its corpus reading
//! is issue 241 Step 2 (`capture_196177`), where the second attempt's `200` rode
//! a SECOND caller-facing tag.
//!
//! **Issue 247 split the two halves and these rungs pin the split.** In
//! TRANSPARENT mode (no `relayFirst18xTo180` arm — what these rungs run) the
//! caller's dialog set mirrors the callee's, so a rerouted attempt's RINGING
//! opens a caller-facing early dialog of its own, exactly as a second fork on
//! one leg does. Its REJECTION does not: a non-2xx final ends the transaction
//! and every early dialog it created, and `sip-txn` has already pinned that
//! tag from the first >100 response, so both finals of one transaction agree.
//! Under a masking arm the whole hunt still fuses onto one caller identity
//! — `capture_196177` is such a call, which
//! is why its divergence stays ruled rather than fixed.

use std::sync::Arc;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{CallFailureResponse, NewCallResponse, ScriptedDecisionEngine};
use b2bua_harness::{settle_until, B2buaSut};
use call::CdrEventType;
use scenario_harness::Harness;
use std::net::SocketAddr;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";

/// A B2BUA that routes to `first_port`, fails the first b-leg over to
/// `second_port` under `new_ruri`, and RELAYS any further failure to the caller
/// (the failover plan is one deep). Mirrors the wire
/// `on_failure: { action: "failover", destination, new_ruri }`.
fn reroute_once(first_port: u16, second_port: u16, new_ruri: &'static str) -> Arc<ScriptedDecisionEngine> {
    Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| {
                let mut r = route_to("127.0.0.1", first_port);
                // The failover-capable marker: without a callback context the
                // b-leg failure is relayed instead of consulted.
                r.callback_context = Some("reroute-test".into());
                NewCallResponse::Route(r)
            })
            .on_failure(move |req| {
                if req.failure.failed_leg_id.as_deref() == Some("b-1") {
                    let mut r = route_to("127.0.0.1", second_port);
                    r.new_ruri = Some(new_ruri.to_string());
                    r.callback_context = Some("reroute-test".into());
                    CallFailureResponse::Route(r)
                } else {
                    CallFailureResponse::Relay
                }
            })
            .build(),
    )
}

/// The To-tag on the ACK the caller's transaction layer put on the wire for
/// `cseq` (§17.1.1.3) — read off the recording, since the auto-ACK is the txn
/// layer's own and never surfaces to the body.
fn caller_ack_to_tag(h: &Harness, from: SocketAddr, to: SocketAddr, cseq: u32) -> String {
    h.wire_entries()
        .into_iter()
        .find(|e| {
            e.from == from
                && e.to == to
                && e.raw.starts_with(b"ACK ")
                && sip_message::sniff::cseq_number(&e.raw) == Some(cseq)
        })
        .map(|e| sip_message::sniff::to_tag(&e.raw))
        .expect("the caller's txn layer ACKed the non-2xx final")
}

/// The rerouted call left one CDR carrying a reject, and the B2BUA holds nothing.
fn assert_rejected_and_reaped(b2bua: &B2buaSut) {
    let cdrs = b2bua.cdr_records();
    assert_eq!(cdrs.len(), 1, "one CDR for the rerouted-then-rejected call");
    let kinds: Vec<CdrEventType> = cdrs[0].events.iter().map(|e| e.event_type).collect();
    assert!(kinds.contains(&CdrEventType::Reject), "reject event: {kinds:?}");
    assert_eq!(cdrs[0].b_legs.len(), 2, "both attempts are on the record");
    b2bua.assert_fully_reaped();
}

/// Attempt 1 rings, then rejects; the reroute rings a second callee, which
/// rejects too, and THAT rejection is the one the caller sees. The two halves
/// part company, and the split is the subject (issue 247):
///
/// - attempt 2's **ringing** opens the caller's SECOND early dialog. In
///   transparent mode her dialog set mirrors the callee's, and a rerouted leg
///   is a different callee — the tag the platform derives from the callee it
///   reached cannot survive reaching another one.
/// - attempt 2's **rejection** rides attempt 1's tag. A non-2xx final ends the
///   transaction and every early dialog it created (RFC 3261 §13.2.2.3), so it
///   establishes nothing and names no dialog the caller keeps; `sip-txn` has
///   already pinned that tag from the first >100 response (§17.2.1) and
///   generates the autonomous 487 with it, so moving the relayed final would
///   split the two finals of one transaction across two tags.
#[tokio::test]
async fn a_rerouted_attempt_s_rejection_rides_the_first_attempt_s_caller_tag() {
    const ALICE: &str = "127.0.0.1:6051";
    const B2BUA: &str = "127.0.0.1:6054";
    let h = Harness::with_transit_delay("b2bua-reroute-rejection-primary-tag", 1);
    let alice = h.agent("alice", ALICE).await;
    let bob1 = h.agent("bob1", "127.0.0.1:6052").await;
    let bob2 = h.agent("bob2", "127.0.0.1:6053").await;
    let b2bua = B2buaSut::builder(reroute_once(6052, 6053, "sip:+1234@127.0.0.1:6053"))
        .start(&h, "b2bua", B2BUA)
        .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;

    // ── attempt 1 rings, minting the caller's early dialog, then rejects ─────
    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    let p1 = call.expect(180).await;
    let first_atag = p1.to().tag().expect("attempt 1 a-facing tag").to_string();

    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await; // the b2bua completes bob1's reject txn (§17.1.1.3)

    // ── attempt 2 rings under the SAME caller-facing tag ─────────────────────
    let mut uas2 = bob2.receive("INVITE").await;
    assert_eq!(
        uas2.request().request_uri().text(),
        "sip:+1234@127.0.0.1:6053",
        "the second attempt carries the failover R-URI",
    );
    uas2.respond(180, "Ringing").await;
    let p2 = call.expect(180).await;
    assert_ne!(
        p2.to().tag(),
        Some(first_atag.as_str()),
        "transparent mode mirrors the callee's dialogs: a rerouted leg's ring opens \
         the caller's SECOND early dialog, not a second provisional in her first",
    );

    // ── attempt 2 rejects, and the caller finally sees a final ───────────────
    uas2.respond(486, "Busy Here").await;
    bob2.receive("ACK").await;
    let rejected = call.expect(486).await;
    assert_eq!(
        rejected.to().tag(),
        Some(first_atag.as_str()),
        "the caller-facing final rides the tag the FIRST attempt minted",
    );
    assert_eq!(
        caller_ack_to_tag(&h, ALICE.parse().unwrap(), B2BUA.parse().unwrap(), 1),
        first_atag,
        "the caller's ACK copies the To of the final it acknowledges (§17.1.1.3)",
    );

    settle_until(|| !b2bua.cdr_records().is_empty() && b2bua.active_calls() == 0).await;
    assert_rejected_and_reaped(&b2bua);
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    let _report = h.finish().await;
}

/// The same reroute, with the SECOND attempt forking: the caller ends up holding
/// THREE early dialogs — attempt 1's, and one per fork of attempt 2 — because
/// the leg swap and the fork are the same rule in transparent mode, one
/// a-facing dialog per callee early dialog. The rejection, sent by the non-first
/// fork of the second attempt, still rides the PRIMARY tag: a final names no
/// dialog the caller keeps, however many she was shown.
#[tokio::test]
async fn a_rerouted_fork_s_rejection_still_rides_the_first_attempt_s_caller_tag() {
    const ALICE: &str = "127.0.0.1:6055";
    const B2BUA: &str = "127.0.0.1:6058";
    let h = Harness::with_transit_delay("b2bua-reroute-forked-rejection-primary-tag", 1);
    let alice = h.agent("alice", ALICE).await;
    let bob1 = h.agent("bob1", "127.0.0.1:6056").await;
    let bob2 = h.agent("bob2", "127.0.0.1:6057").await;
    let b2bua = B2buaSut::builder(reroute_once(6056, 6057, "sip:+1234@127.0.0.1:6057"))
        .start(&h, "b2bua", B2BUA)
        .await;

    let mut call = alice.invite(&bob1).with_sdp(OFFER).through(b2bua.addr).send().await;

    let mut uas1 = bob1.receive("INVITE").await;
    uas1.respond(180, "Ringing").await;
    let p1 = call.expect(180).await;
    let first_atag = p1.to().tag().expect("attempt 1 a-facing tag").to_string();
    uas1.respond(503, "Service Unavailable").await;
    bob1.receive("ACK").await;

    // ── attempt 2 forks: each fork is its own caller-facing early dialog ─────
    let mut uas2 = bob2.receive("INVITE").await;
    uas2.respond(180, "Ringing").with_to_tag("bob2fork1").await;
    let p2 = call.expect(180).await;
    let fork1_atag = p2.to().tag().expect("fork1 a-facing tag").to_string();
    assert_ne!(
        fork1_atag, first_atag,
        "the rerouted leg's FIRST fork is a new callee dialog, so it opens a new \
         caller one — the leg swap mirrors like any other fork",
    );

    uas2.respond(180, "Ringing").with_to_tag("bob2fork2").await;
    let p3 = call.expect(180).await;
    let fork2_atag = p3.to().tag().expect("fork2 a-facing tag").to_string();
    assert_ne!(
        fork2_atag, first_atag,
        "the rerouted leg's SECOND fork mints a caller-facing early dialog of its own",
    );
    assert_ne!(
        fork2_atag, fork1_atag,
        "one a-facing dialog per callee early dialog: the two forks never share a tag",
    );

    // ── that non-first fork rejects ──────────────────────────────────────────
    uas2.respond(486, "Busy Here").with_to_tag("bob2fork2").await;
    let b_ack = bob2.receive("ACK").await;
    assert_eq!(
        b_ack.request().to().tag(),
        Some("bob2fork2"),
        "the B2BUA's b-facing ACK acknowledges the response where it was generated",
    );

    let rejected = call.expect(486).await;
    assert_eq!(
        rejected.to().tag(),
        Some(first_atag.as_str()),
        "neither the leg swap nor the fork moves it: the caller-facing final \
         rides the leg's PRIMARY tag",
    );
    assert_eq!(
        caller_ack_to_tag(&h, ALICE.parse().unwrap(), B2BUA.parse().unwrap(), 1),
        first_atag,
        "the caller's ACK copies the To of the final it acknowledges (§17.1.1.3)",
    );

    settle_until(|| !b2bua.cdr_records().is_empty() && b2bua.active_calls() == 0).await;
    assert_rejected_and_reaped(&b2bua);
    alice.drain().await;
    bob1.drain().await;
    bob2.drain().await;
    let _report = h.finish().await;
}
