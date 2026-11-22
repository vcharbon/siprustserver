//! A release reroute whose a-realign comes due while the caller's ACK is in
//! flight (RFC 3261 §14.1).
//!
//! A re-INVITEs B, B answers 200 and the stack relays that 2xx to A; A's ACK
//! of it is in flight when the call's maximum duration fires. The release
//! consult answers with a route to C, and C answers the replacement INVITE
//! inside that window. The 2xx this stack sent A awaits her ACK, so an INVITE
//! transaction is in progress on her dialog: the a-realign re-INVITE waits
//! for her ACK and leaves on it, carrying C's answer, and the call goes on
//! A↔C. A repeat of her ACK, or a copy of her earlier one, releases nothing
//! more, and a copy of an earlier 2xx of hers completes no reroute. Where B
//! re-INVITEs instead and his ACK of A's 2xx is in flight, her dialog awaits
//! that ACK: the a-realign re-INVITE leaves once the stack relayed it to her.
//! Where her ACK never comes, the reroute guard ends the call.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use b2bua::decision::test_adapter::route_to;
use b2bua::decision::{
    CallReleaseResponse, NewCallResponse, ReleaseOutcome, ScriptedDecisionEngine,
};
use b2bua_harness::{settle_until, B2buaSut};
use call::{CdrEventType, ReleaseEventKind};

use crate::common::held_acks::{acks_held, reinvite_acks_held};
use crate::common::sdp::apart_from_origin;
use scenario_harness::{Agent, Dialog, Harness, ServerTxn};
use sip_message::generators::InDialogMethod;
use sip_net_testkit::RecordedSipEntry;

/// A's offer in her INVITE.
const ALICE_OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// B's answer to [`ALICE_OFFER`].
const BOB_ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's description in the re-INVITE exchange before the cap, her session one
/// version up: her offer, or her answer where B re-INVITEs.
const ALICE_SECOND: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// B's description in that exchange, his session one version up: his
/// answer, or his offer where he re-INVITEs.
const BOB_SECOND: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// C's answer to the replacement INVITE, which carries [`ALICE_OFFER`].
const CAROL_ANSWER: &str = "v=0\r\no=carol 7 7 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 30000 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";
/// A's answer to the a-realign re-INVITE, her session one version past
/// [`ALICE_SECOND`].
const ALICE_REALIGN_ANSWER: &str = "v=0\r\no=alice 1 3 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10004 RTP/AVP 8\r\na=rtpmap:8 PCMA/8000\r\na=sendrecv\r\n";

/// RFC 3261's T1: the first rung of the stack's 2xx ladder toward A
/// (§13.3.1.4).
const T1: Duration = Duration::from_millis(500);

/// The maximum call duration of the A↔B route.
const CAP: Duration = Duration::from_secs(60);

/// How long before [`CAP`] A re-INVITEs: her re-INVITE's round trip ends
/// first, and the cap fires while her ACK is held.
const LEAD: Duration = Duration::from_millis(100);

/// How long the fabric holds the re-inviter's ACK of the 2xx to the re-INVITE
/// before the cap: past [`LEAD`], the release consult and C's round trip on
/// the harness's 1 ms hops, inside T1, so no 2xx is re-sent meanwhile.
const ACK_HELD: Duration = Duration::from_millis(300);

/// How long the fabric holds the re-inviter's first ACK when it is repeated:
/// past the T1 copy of the stack's 2xx, which draws the second ACK.
const FIRST_ACK_HELD_PAST_T1: Duration = Duration::from_millis(800);

/// How long the fabric holds the re-inviter's second ACK: it arrives after
/// the first has released the a-realign, while that re-INVITE awaits A's
/// final, and before the stack's next 2xx copy at 3·T1.
const REPEAT_HELD: Duration = Duration::from_millis(400);

/// Past the reroute's round trips on the harness's 1 ms hops, inside
/// [`ACK_HELD`] counted from A's ACK.
const PAST_THE_REROUTES_ROUND_TRIPS: Duration = Duration::from_millis(20);

/// The reroute guard where A's ACK never comes: inside the stack's own 64·T1
/// wait for that ACK, and clear of its 2xx copies at 7.5 s and 11.5 s.
const GUARD: Duration = Duration::from_secs(10);

/// The defect the one-INVITE guard reports for a refused re-INVITE (ADR-0064).
const DEFECT: &str = "one-invite-in-progress-per-dialog";

/// The parties once C has answered the replacement INVITE, with the ACK of
/// the 2xx to the re-INVITE before the cap not yet at the stack.
struct ARealignDue {
    h: Harness,
    b2bua: B2buaSut,
    alice: Agent,
    bob: Agent,
    carol: Agent,
    alice_dialog: Dialog,
    /// Who sent the re-INVITE before the cap.
    reinviter: SocketAddr,
    /// The CSeq number of that re-INVITE, which its ACK carries.
    reinvite_cseq: u32,
}

impl ARealignDue {
    /// Every datagram `from` sent `to` whose start line begins `start`, on
    /// CSeq number `cseq` when one is given, first sends only (no re-emitted
    /// copy), with its place on the wire.
    fn sent(
        &self,
        from: SocketAddr,
        to: SocketAddr,
        start: &str,
        cseq: Option<u32>,
    ) -> Vec<(usize, RecordedSipEntry)> {
        self.h
            .wire_entries()
            .into_iter()
            .enumerate()
            .filter(|(_, e)| e.from == from && e.to == to && e.reemit.is_none())
            .filter(|(_, e)| e.raw.starts_with(start.as_bytes()))
            .filter(|(_, e)| {
                cseq.is_none_or(|n| sip_message::sniff::cseq_number(&e.raw) == Some(n))
            })
            .collect()
    }

    /// Every INVITE the stack sent A once it answered the re-INVITE before
    /// the cap: the a-realign re-INVITEs.
    fn invites_to_alice(&self) -> Vec<(usize, RecordedSipEntry)> {
        let answered =
            self.sent(self.b2bua.addr, self.reinviter, "SIP/2.0 200 ", Some(self.reinvite_cseq));
        let after = answered.first().map_or(0, |(at, _)| *at);
        let invites = self.sent(self.b2bua.addr, self.alice.addr(), "INVITE ", None);
        invites.into_iter().filter(|(at, _)| *at > after).collect()
    }

    /// Every ACK the re-inviter sent of the 2xx to the re-INVITE before the
    /// cap.
    fn reinviters_acks(&self) -> Vec<(usize, RecordedSipEntry)> {
        self.sent(self.reinviter, self.b2bua.addr, "ACK ", Some(self.reinvite_cseq))
    }

    /// Every ACK the stack sent C.
    fn acks_to_carol(&self) -> Vec<(usize, RecordedSipEntry)> {
        self.sent(self.b2bua.addr, self.carol.addr(), "ACK ", None)
    }

    /// No re-INVITE was refused so far: the one-INVITE guard reported nothing.
    fn assert_no_refusal(&mut self) {
        let refused: Vec<String> = self
            .b2bua
            .take_model_violations()
            .iter()
            .map(|v| v.lines().next().unwrap_or_default().to_string())
            .collect();
        assert!(
            refused.is_empty(),
            "the a-realign re-INVITE waits for A's ACK, never asked while her 2xx \
             awaits it ({DEFECT}): {refused:?}"
        );
    }
}

/// The A↔B route: [`CAP`], its expiry subscribed to the release consult.
fn route_to_bob(bob_port: u16) -> b2bua::decision::RouteDecision {
    let mut r = route_to("127.0.0.1", bob_port);
    r.features.platform.max_duration_ms = CAP.as_millis() as i64;
    r.callback_context = Some("ctx-release".into());
    r.subscriptions = vec![ReleaseEventKind::MaxCallDuration];
    r
}

/// Who re-INVITEs [`LEAD`] before the cap, with an offer the other party
/// answers 200.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ReInviter {
    /// A: the stack relays B's 2xx to her and her dialog awaits her ACK.
    Caller,
    /// B: the stack relays A's 2xx to him and her dialog awaits his ACK,
    /// which the stack relays to her.
    Callee,
}

/// How the call runs up to C's answer.
#[derive(Clone, Copy)]
struct Setup {
    /// Who re-INVITEs before the cap.
    reinviter: ReInviter,
    /// How the fabric holds the re-inviter's ACKs of that re-INVITE's 2xx.
    holds: &'static [Duration],
    /// Whether the re-inviter ACKs that 2xx.
    acks: bool,
    /// The reroute guard, where the stack's default is not the one tested.
    guard: Option<Duration>,
    /// Whether B refreshes the session first: his re-INVITE re-offers his
    /// description unchanged, A answers hers unchanged (RFC 3264 §8) and B
    /// ACKs at once. Only where A re-INVITEs before the cap, so the fabric
    /// holds none of B's ACKs.
    refresh: bool,
}

impl Setup {
    /// A re-INVITEs and ACKs the 2xx over a fabric holding her ACKs as
    /// `holds` says; no refresh, the default guard.
    fn alice_acks_held(holds: &'static [Duration]) -> Self {
        Setup { reinviter: ReInviter::Caller, holds, acks: true, guard: None, refresh: false }
    }

    /// [`Setup::alice_acks_held`] where B re-INVITEs, his ACKs held.
    fn bob_acks_held(holds: &'static [Duration]) -> Self {
        Setup { reinviter: ReInviter::Callee, ..Setup::alice_acks_held(holds) }
    }
}

/// A INVITEs B and the call is set up, its release consult answering with a
/// route to C; B refreshes the session where `setup` says. [`LEAD`] before
/// the cap, the re-inviter `setup` names re-INVITEs with an offer, the other
/// party answers 200 and the re-inviter receives it and ACKs it as `setup`
/// says. The cap fires, the stack dials C with A's offer, C answers and is
/// ACKed.
async fn a_realign_due(name: &str, n: u16, setup: Setup) -> ARealignDue {
    let (alice_port, bob_port, b2bua_port, carol_port) = (5741 + n, 5751 + n, 5761 + n, 5771 + n);
    let alice_addr: SocketAddr = format!("127.0.0.1:{alice_port}").parse().expect("an address");
    let bob_addr: SocketAddr = format!("127.0.0.1:{bob_port}").parse().expect("an address");
    let (reinviter, held) = match setup.reinviter {
        ReInviter::Caller => (alice_addr, reinvite_acks_held(alice_addr, setup.holds)),
        ReInviter::Callee => (bob_addr, acks_held(bob_addr, setup.holds)),
    };
    let h = Harness::with_transit_delay_and_send_delay(name, 1, held);
    let alice = h.agent("alice", &alice_addr.to_string()).await;
    let bob = h.agent("bob", &bob_addr.to_string()).await;
    let carol = h.agent("carol", &format!("127.0.0.1:{carol_port}")).await;
    let decision = Arc::new(
        ScriptedDecisionEngine::builder()
            .fallback(move |_req| NewCallResponse::Route(route_to_bob(bob_port)))
            .on_release(move |_req| {
                ReleaseOutcome::Respond(CallReleaseResponse::Route(route_to(
                    "127.0.0.1",
                    carol_port,
                )))
            })
            .build(),
    );
    let b2bua = B2buaSut::builder(decision)
        .tune(move |c| {
            c.keepalive_interval_sec = 3_600;
            c.reaper_enabled = false;
            if let Some(guard) = setup.guard {
                c.release_reroute_guard_sec = guard.as_secs() as i64;
            }
        })
        .start(&h, "b2bua", &format!("127.0.0.1:{b2bua_port}"))
        .await;

    let mut call = alice.invite(&bob).with_sdp(ALICE_OFFER).through(b2bua.addr).send().await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(BOB_ANSWER).await;
    call.expect(200).await;
    let mut alice_dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bob_dialog = bob_uas.dialog();
    if setup.refresh {
        let mut refresh = bob_dialog.request(InDialogMethod::Invite, Some(BOB_ANSWER)).await;
        alice.receive("INVITE").await.respond(200, "OK").with_sdp(ALICE_OFFER).await;
        refresh.expect(200).await;
        bob_dialog.ack(None).await;
        alice.receive("ACK").await;
    }

    h.advance(CAP - LEAD).await;
    let reinvite_cseq = match setup.reinviter {
        ReInviter::Caller => {
            let mut reinvite =
                alice_dialog.request(InDialogMethod::Invite, Some(ALICE_SECOND)).await;
            let cseq = reinvite.sent_invite().expect("A's re-INVITE").cseq().seq();
            bob.receive("INVITE").await.respond(200, "OK").with_sdp(BOB_SECOND).await;
            reinvite.expect(200).await;
            if setup.acks {
                alice_dialog.ack(None).await;
            }
            cseq
        }
        ReInviter::Callee => {
            let mut reinvite = bob_dialog.request(InDialogMethod::Invite, Some(BOB_SECOND)).await;
            let cseq = reinvite.sent_invite().expect("B's re-INVITE").cseq().seq();
            alice.receive("INVITE").await.respond(200, "OK").with_sdp(ALICE_SECOND).await;
            reinvite.expect(200).await;
            if setup.acks {
                bob_dialog.ack(None).await;
            }
            cseq
        }
    };

    let mut carol_uas = carol.receive("INVITE").await;
    assert_eq!(
        apart_from_origin(&String::from_utf8_lossy(carol_uas.request().body())),
        apart_from_origin(ALICE_OFFER),
        "the replacement INVITE carries A's offer"
    );
    carol_uas.respond(200, "OK").with_sdp(CAROL_ANSWER).await;
    carol.receive("ACK").await;
    ARealignDue { h, b2bua, alice, bob, carol, alice_dialog, reinviter, reinvite_cseq }
}

/// The call's CDR events, as `(type, reason)` in the order written.
fn cdr_events(b2bua: &B2buaSut) -> Vec<(CdrEventType, Option<String>)> {
    b2bua
        .cdr_records()
        .iter()
        .flat_map(|r| r.events.iter().map(|e| (e.event_type, e.reason.clone())))
        .collect()
}

/// A receives the a-realign re-INVITE, carrying C's answer, and answers it:
/// the stack ACKs her, BYEs B and the reroute completes. A's BYE then ends
/// the call on C, and the call reaps fully.
async fn reroute_completes(mut t: ARealignDue, mut a_realign: ServerTxn) {
    assert_eq!(
        apart_from_origin(&String::from_utf8_lossy(a_realign.request().body())),
        apart_from_origin(CAROL_ANSWER),
        "the a-realign re-INVITE carries C's answer"
    );
    a_realign.respond(200, "OK").with_sdp(ALICE_REALIGN_ANSWER).await;
    t.alice.receive("ACK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    let mut alice_bye = t.alice_dialog.bye().await;
    t.carol.receive("BYE").await.respond(200, "OK").await;
    alice_bye.expect(200).await;

    t.assert_no_refusal();
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    assert_eq!(t.acks_to_carol().len(), 1, "C receives the ACK of her own 2xx alone");
    let events = cdr_events(&t.b2bua);
    assert!(
        events.iter().any(|(_, r)| r.as_deref() == Some("release-reroute-completed")),
        "the reroute completes: {events:?}"
    );
    let _ = t.h.finish().await;
}

/// A's ACK of her re-INVITE's 2xx is in flight while the cap fires and C
/// answers the replacement INVITE: the a-realign re-INVITE does not leave
/// while the stack's 2xx to A awaits that ACK, and leaves once it arrives,
/// the ACK relayed to B; the reroute completes.
#[tokio::test(start_paused = true)]
async fn the_a_realign_leaves_on_the_callers_ack() {
    let setup = Setup::alice_acks_held(&[ACK_HELD]);
    let mut t = a_realign_due("reroute-realign-awaits-ack", 0, setup).await;

    t.h.advance(PAST_THE_REROUTES_ROUND_TRIPS).await;
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE before A's ACK arrives");

    t.bob.receive("ACK").await;
    let a_realign = t.alice.receive("INVITE").await;
    let acks = t.reinviters_acks();
    let invites = t.invites_to_alice();
    assert_eq!((acks.len(), invites.len()), (1, 1), "one ACK of A's, one a-realign re-INVITE");
    let arrived_ms = acks[0].1.sent_ms + ACK_HELD.as_millis() as u64;
    assert!(
        invites[0].1.sent_ms >= arrived_ms,
        "the a-realign re-INVITE leaves on A's ACK: sent at {} ms, the ACK arrived at {arrived_ms} ms",
        invites[0].1.sent_ms
    );
    reroute_completes(t, a_realign).await;
}

/// While A's ACK of her re-INVITE's 2xx is in flight, the fabric delivers a
/// copy of her earlier ACK, the one of her INVITE's 2xx: it discharges
/// nothing, so the 2xx to her re-INVITE still awaits its ACK and the
/// a-realign re-INVITE goes on waiting. The held ACK then releases it.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_earlier_ack_releases_nothing() {
    let name = "reroute-realign-awaits-ack-earlier-copy";
    let mut t = a_realign_due(name, 1, Setup::alice_acks_held(&[ACK_HELD])).await;

    let earlier = t.sent(t.alice.addr(), t.b2bua.addr, "ACK ", None);
    let earlier = earlier.first().expect("A's ACK of her INVITE's 2xx").1.raw.clone();
    t.alice.try_send_datagram(&earlier, t.b2bua.addr).await.expect("the copy leaves");
    t.h.advance(PAST_THE_REROUTES_ROUND_TRIPS).await;
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "the earlier ACK's copy releases no re-INVITE");
    t.bob.drain().await;

    let a_realign = t.alice.receive("INVITE").await;
    assert_eq!(t.invites_to_alice().len(), 1, "the held ACK releases the a-realign re-INVITE");
    t.bob.drain().await;
    reroute_completes(t, a_realign).await;
}

/// B refreshed the session earlier. While A's ACK of her re-INVITE's 2xx is
/// in flight, the fabric delivers a copy of A's 2xx to B's refresh (RFC 3261
/// §13.3.1.4): it answers no re-INVITE of the reroute, so it completes
/// nothing and B keeps the call. The held ACK then releases the a-realign
/// re-INVITE, and A's answer to it completes the reroute.
#[tokio::test(start_paused = true)]
async fn a_copy_of_an_earlier_2xx_completes_no_reroute() {
    let name = "reroute-realign-awaits-ack-earlier-2xx";
    let setup = Setup { refresh: true, ..Setup::alice_acks_held(&[ACK_HELD]) };
    let mut t = a_realign_due(name, 4, setup).await;

    let earlier = t.sent(t.alice.addr(), t.b2bua.addr, "SIP/2.0 200 ", None);
    let earlier = earlier.first().expect("A's 2xx to B's refresh").1.raw.clone();
    t.alice.try_send_datagram(&earlier, t.b2bua.addr).await.expect("the copy leaves");
    t.h.advance(PAST_THE_REROUTES_ROUND_TRIPS).await;
    t.assert_no_refusal();
    let byes_to_bob = t.sent(t.b2bua.addr, t.bob.addr(), "BYE ", None);
    assert!(byes_to_bob.is_empty(), "the 2xx copy completes no reroute: B keeps the call");
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE before A's ACK arrives");
    t.alice.drain().await;

    t.bob.receive("ACK").await;
    let a_realign = t.alice.receive("INVITE").await;
    assert_eq!(t.invites_to_alice().len(), 1, "the held ACK releases the a-realign re-INVITE");
    reroute_completes(t, a_realign).await;
}

/// The fabric holds A's ACK past T1: the stack re-sends its 2xx, A ACKs the
/// copy too, and her two ACKs arrive in turn. The first releases the
/// a-realign re-INVITE; the second, arriving while that re-INVITE awaits A's
/// final, releases nothing more.
#[tokio::test(start_paused = true)]
async fn a_repeated_ack_releases_the_a_realign_once() {
    let name = "reroute-realign-awaits-ack-repeat";
    let holds = &[FIRST_ACK_HELD_PAST_T1, REPEAT_HELD];
    let mut t = a_realign_due(name, 2, Setup::alice_acks_held(holds)).await;

    t.h.advance(T1).await;
    t.alice.drain_tu().await;
    assert_eq!(t.reinviters_acks().len(), 2, "A ACKs the stack's 2xx copy as well");
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE before A's ACK arrives");

    t.bob.receive("ACK").await;
    let a_realign = t.alice.receive("INVITE").await;
    t.h.advance(REPEAT_HELD).await;
    t.assert_no_refusal();
    assert_eq!(t.invites_to_alice().len(), 1, "A's repeated ACK releases no second re-INVITE");
    t.bob.drain().await;
    reroute_completes(t, a_realign).await;
}

/// B re-INVITEs before the cap, and his ACK of A's 2xx is in flight while the
/// cap fires and C answers the replacement INVITE: A's dialog awaits that
/// ACK, which the stack relays to her, so the a-realign re-INVITE does not
/// leave before it, and leaves once B's ACK is relayed to A; the reroute
/// completes.
#[tokio::test(start_paused = true)]
async fn the_a_realign_leaves_on_the_callees_ack_relayed_to_the_caller() {
    let name = "reroute-realign-awaits-callee-ack";
    let mut t = a_realign_due(name, 5, Setup::bob_acks_held(&[ACK_HELD])).await;

    t.h.advance(PAST_THE_REROUTES_ROUND_TRIPS).await;
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE before B's ACK arrives");

    t.alice.receive("ACK").await;
    let a_realign = t.alice.receive("INVITE").await;
    let acks = t.reinviters_acks();
    let invites = t.invites_to_alice();
    assert_eq!((acks.len(), invites.len()), (1, 1), "one ACK of B's, one a-realign re-INVITE");
    let relayed = t.sent(t.b2bua.addr, t.alice.addr(), "ACK ", None);
    let relayed_at = relayed.last().expect("B's ACK relayed to A").0;
    assert!(
        relayed_at < invites[0].0,
        "the a-realign re-INVITE leaves after B's ACK, relayed to A"
    );
    reroute_completes(t, a_realign).await;
}

/// The fabric holds B's ACK past T1: the stack re-sends A's 2xx to him, B
/// ACKs the copy too, and his two ACKs arrive in turn. The first, relayed to
/// A, releases the a-realign re-INVITE; the second, arriving while that
/// re-INVITE awaits A's final, releases nothing more.
#[tokio::test(start_paused = true)]
async fn a_repeated_callee_ack_releases_the_a_realign_once() {
    let name = "reroute-realign-awaits-callee-ack-repeat";
    let holds = &[FIRST_ACK_HELD_PAST_T1, REPEAT_HELD];
    let mut t = a_realign_due(name, 6, Setup::bob_acks_held(holds)).await;

    t.h.advance(T1).await;
    t.bob.drain_tu().await;
    assert_eq!(t.reinviters_acks().len(), 2, "B ACKs the stack's 2xx copy as well");
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE before B's ACK arrives");

    t.alice.receive("ACK").await;
    let a_realign = t.alice.receive("INVITE").await;
    t.h.advance(REPEAT_HELD).await;
    t.assert_no_refusal();
    assert_eq!(t.invites_to_alice().len(), 1, "B's repeated ACK releases no second re-INVITE");
    reroute_completes(t, a_realign).await;
}

/// A's ACK of her re-INVITE's 2xx never comes: the a-realign re-INVITE never
/// leaves. The reroute guard fires [`GUARD`] after the release consult's
/// answer and ends the call: A, B and C are BYEd, and the CDR names the
/// reroute's timeout.
#[tokio::test(start_paused = true)]
async fn the_guard_ends_a_reroute_still_waiting_for_the_callers_ack() {
    let name = "reroute-realign-awaits-ack-guard";
    let setup = Setup { acks: false, guard: Some(GUARD), ..Setup::alice_acks_held(&[]) };
    let mut t = a_realign_due(name, 3, setup).await;

    t.h.advance(GUARD - Duration::from_millis(500)).await;
    t.assert_no_refusal();
    assert!(t.invites_to_alice().is_empty(), "no a-realign re-INVITE without A's ACK");
    t.alice.drain().await;
    t.bob.drain().await;

    t.h.advance(Duration::from_secs(1)).await;
    t.alice.receive("BYE").await.respond(200, "OK").await;
    t.bob.receive("ACK").await;
    t.bob.receive("BYE").await.respond(200, "OK").await;
    t.carol.receive("BYE").await.respond(200, "OK").await;

    t.assert_no_refusal();
    settle_until(|| t.b2bua.is_reaped()).await;
    t.b2bua.assert_fully_reaped();
    assert!(t.invites_to_alice().is_empty(), "the a-realign re-INVITE never left");
    let events = cdr_events(&t.b2bua);
    assert!(
        events
            .iter()
            .any(|(e, r)| *e == CdrEventType::Timeout
                && r.as_deref() == Some("release-reroute-timeout")),
        "the reroute guard ends the call: {events:?}"
    );
    t.alice.drain().await;
    let _ = t.h.finish().await;
}
