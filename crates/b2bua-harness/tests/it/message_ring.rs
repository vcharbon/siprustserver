//! The per-leg message ring on the replicated `Call`: one entry per distinct
//! SIP message a leg received or sent, in handling order, with the values of
//! the configured headers — and nothing for a retransmission, inbound or
//! outbound (RFC 3261 §17.2.1 replays, §13.2.2.4 re-ACKs, §13.3.1.4 2xx
//! repeats). The cap keeps the last N entries and counts the evicted; a cap of
//! `0` (the default) records nothing.
//!
//! The SUT is spawned bare so a probe [`CdrWriter`] hands the test the
//! terminated `Call` — the ring as the record sees it — while the live copy
//! is read mid-call for the steady-state body measurement, encoded the way the
//! store flushes it (`MsgpackCodec`).

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use b2bua::cdr::{CdrRecord, CdrWriter, InMemoryCdrWriter};
use b2bua::config::{B2buaConfig, CdrConfig};
use b2bua::decision::ScriptedDecisionEngine;
use b2bua::limiter::NoopLimiter;
use b2bua::metrics::B2buaMetrics;
use b2bua::store::InMemoryCallStore;
use b2bua::{B2buaCore, B2buaDeps};
use b2bua_harness::settle_until;
use call::{Call, CallBodyCodec, MessageDirection, MessageEntry, MsgpackCodec};
use scenario_harness::{Agent, Harness, WaiverScope};
use sip_clock::Clock;
use sip_message::generators::InDialogMethod;
use sip_txn::IdGen;

const OFFER: &str = "v=0\r\no=alice 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10000 RTP/AVP 0\r\n";
const ANSWER: &str = "v=0\r\no=bob 1 1 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20000 RTP/AVP 0\r\n";
const REOFFER: &str = "v=0\r\no=alice 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 10002 RTP/AVP 0\r\n";
const REANSWER: &str = "v=0\r\no=bob 1 2 IN IP4 127.0.0.1\r\ns=-\r\nc=IN IP4 127.0.0.1\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

const ALICE: &str = "127.0.0.1:5060";
const BOB: &str = "127.0.0.1:5070";
const B2BUA: &str = "127.0.0.1:5080";
const ORDINAL: &str = "w0";

/// The terminated calls the SUT wrote, as the record saw them.
#[derive(Clone, Default)]
struct TerminatedCalls(Arc<Mutex<Vec<Call>>>);

impl TerminatedCalls {
    fn snapshot(&self) -> Vec<Call> {
        self.0.lock().unwrap().clone()
    }
}

struct ProbeCdr {
    inner: InMemoryCdrWriter,
    terminated: TerminatedCalls,
}

#[async_trait]
impl CdrWriter for ProbeCdr {
    async fn write(&self, call: &Call, terminated_at: i64) {
        self.terminated.0.lock().unwrap().push(call.clone());
        self.inner.write(call, terminated_at).await;
    }
    async fn read_all(&self) -> Vec<CdrRecord> {
        self.inner.read_all().await
    }
}

/// A bare SUT routing everything to bob, its ring configured by `cdr`.
struct Sut {
    addr: SocketAddr,
    core: B2buaCore,
    terminated: TerminatedCalls,
}

impl Sut {
    async fn spawn(h: &Harness, cdr: CdrConfig) -> Self {
        Self::spawn_tuned(h, cdr, |_| {}).await
    }

    async fn spawn_tuned(h: &Harness, cdr: CdrConfig, tune: impl FnOnce(&mut B2buaConfig)) -> Self {
        // A UA on both faces, as `B2buaSut` declares it; the harness baseline
        // tuning (the keepalive cadence, the panic-ELU backstop off under a
        // paused clock) as `B2buaSut::start` applies it.
        let (endpoint, addr) = h
            .bind_sut_with_roles(
                "b2bua",
                B2BUA,
                std::collections::HashSet::from([sip_net::UaRole::Uac, sip_net::UaRole::Uas]),
            )
            .await;
        let terminated = TerminatedCalls::default();
        let mut config = B2buaConfig {
            self_ordinal: ORDINAL.into(),
            sip_local_ip: addr.ip().to_string(),
            sip_local_port: addr.port(),
            worker_allowed_target_suffixes: vec!["*".into()],
            keepalive_interval_sec: 30,
            keepalive_timeout_sec: 5,
            overload_panic_elu_threshold: 1.1,
            cdr,
            ..Default::default()
        };
        tune(&mut config);
        let deps = B2buaDeps {
            config,
            decision: Arc::new(ScriptedDecisionEngine::route_all_to("127.0.0.1", 5070)),
            limiter: Arc::new(NoopLimiter),
            cdr: Arc::new(ProbeCdr {
                inner: InMemoryCdrWriter::new(),
                terminated: terminated.clone(),
            }),
            store: Arc::new(InMemoryCallStore::new()),
            store_faults: Default::default(),
            wire_faults: Default::default(),
            clock: Clock::test_at(0),
            id_gen: Arc::new(IdGen::seeded(0xB2B0)),
            replication: None,
            metrics: B2buaMetrics::new(),
            adaptation_http: None,
            compose: b2bua::rules::ComposeOptions::default(),
        };
        let core = B2buaCore::spawn(endpoint, deps);
        Self { addr, core, terminated }
    }

    fn live(&self, call_id: &str, from_tag: &str) -> Call {
        let call_ref = call::derive_call_ref(ORDINAL, call_id, from_tag);
        self.core.live_call(&call_ref).expect("the call is live")
    }

    /// Every call created is reaped and the one CDR is written.
    async fn assert_reaped(&self) -> Call {
        settle_until(|| self.terminated.snapshot().len() == 1).await;
        settle_until(|| self.core.active_calls() == 0).await;
        assert_eq!(self.core.active_calls(), 0, "the call is removed");
        assert_eq!(self.core.lock_count(), 0, "no stranded per-call lock");
        let m = self.core.metrics();
        assert_eq!(m.creations_total(), m.removals_total(), "every call created is removed");
        let terminated = self.terminated.snapshot();
        assert_eq!(terminated.len(), 1, "exactly one CDR per call");
        terminated.into_iter().next().unwrap()
    }
}

fn ring_on() -> CdrConfig {
    CdrConfig {
        message_ring: 32,
        captured_headers: vec!["Allow".into(), "Accept".into(), "Privacy".into()],
    }
}

/// `(direction, method, cseq, code)` of every entry, the shape the
/// assertions read.
fn rows(entries: &[MessageEntry]) -> Vec<(MessageDirection, &str, u32, Option<u16>)> {
    entries.iter().map(|e| (e.direction, e.method.as_str(), e.cseq, e.code)).collect()
}

fn b_leg(call: &Call) -> &call::Leg {
    assert_eq!(call.b_legs.len(), 1, "one b-leg");
    &call.b_legs[0]
}

/// The recorded datagram `from` → `to` whose start line begins with `prefix`.
fn recorded(h: &Harness, from: SocketAddr, to: SocketAddr, prefix: &[u8]) -> Vec<u8> {
    h.wire_entries()
        .into_iter()
        .find(|e| e.from == from && e.to == to && e.raw.starts_with(prefix))
        .map(|e| e.raw)
        .expect("the datagram was recorded")
}

/// Deliver a re-sent datagram and let the SUT handle it: pump the paused
/// clock through the transit hops, draining whatever the peers are handed.
async fn pump(h: &Harness, peers: &[&Agent]) {
    for _ in 0..5 {
        h.advance(Duration::from_millis(100)).await;
        for p in peers {
            p.drain().await;
        }
    }
}

use MessageDirection::{Authored, Received, Relayed};

#[tokio::test(start_paused = true)]
async fn a_basic_call_records_every_distinct_message_on_both_legs() {
    let h = Harness::new("message-ring-basic");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Allow", "INVITE, ACK, BYE")
        .with_header("Privacy", "id")
        .through(sut.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(100, "Trying").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).with_header("Allow", "INVITE, ACK, BYE, CANCEL").await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── Steady state: 5 s after the ACK, the ladders spent ────────────────
    h.advance(Duration::from_secs(5)).await;
    let live = sut.live(&call.call_id(), dialog.local_tag());
    assert_eq!(
        rows(&live.a_leg.messages.entries),
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(180)),
            (Relayed, "INVITE", invite_cseq, Some(200)),
            (Received, "ACK", invite_cseq, None),
        ],
        "the a-leg so far"
    );
    // bob's 100 Trying is the client transaction's (RFC 3261 §17.1.1.2): it
    // moves the transaction to Proceeding and is never handed to the call,
    // so the b-leg ring holds no 100 — the a-leg's is the one this stack
    // sent.
    let b_cseq = b_leg(&live).messages.entries[0].cseq;
    assert_eq!(
        rows(&b_leg(&live).messages.entries),
        vec![
            (Relayed, "INVITE", b_cseq, None),
            (Received, "INVITE", b_cseq, Some(180)),
            (Received, "INVITE", b_cseq, Some(200)),
            (Authored, "ACK", b_cseq, None),
        ],
        "the b-leg so far"
    );
    // The captured values: every configured name, in its order, the lines in
    // wire order; a message carrying none of them captures nothing.
    let a_invite = &live.a_leg.messages.entries[0];
    assert_eq!(
        a_invite.headers,
        vec![
            ("Allow".to_string(), "INVITE, ACK, BYE".to_string()),
            ("Privacy".to_string(), "id".to_string()),
        ]
    );
    assert_eq!(
        b_leg(&live).messages.entries[2].headers,
        vec![("Allow".to_string(), "INVITE, ACK, BYE, CANCEL".to_string())]
    );
    assert!(live.a_leg.messages.entries[4].headers.is_empty(), "the ACK carries none");
    // The To-tags: none on the INVITE and its 100, the dialog's from the 180.
    assert_eq!(a_invite.to_tag, None);
    assert_eq!(live.a_leg.messages.entries[1].to_tag, None);
    assert!(live.a_leg.messages.entries[2].to_tag.is_some(), "the 180 carries the a-dialog tag");

    // ── The replicated body at steady state, ring on: the bytes the store
    // flushes, against the same call with its rings blanked ─────────────
    let codec = MsgpackCodec::new();
    let with_ring = codec.encode(&live).len();
    let mut blank = live.clone();
    blank.a_leg.messages = Default::default();
    for b in &mut blank.b_legs {
        b.messages = Default::default();
    }
    blank.message_seq = 0;
    let without_ring = codec.encode(&blank).len();
    eprintln!(
        "steady-state body: {with_ring} bytes with the ring ({} entries), {without_ring} without, +{}",
        live.a_leg.messages.entries.len() + b_leg(&live).messages.entries.len(),
        with_ring - without_ring
    );
    assert!(
        with_ring - without_ring <= 700,
        "nine entries with three captured names cost {} bytes",
        with_ring - without_ring
    );

    // ── Teardown: alice hangs up; bob's 200 lands after the snapshot ──────
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    bye.expect(200).await;
    let terminating = sut.live(&call.call_id(), dialog.local_tag());
    assert_eq!(
        rows(&terminating.a_leg.messages.entries)[5..],
        [
            (Received, "BYE", dialog.local_cseq(), None),
            (Authored, "BYE", dialog.local_cseq(), Some(200))
        ],
        "the caller's BYE and its answer"
    );
    bob_bye.respond(200, "OK").await;

    let done = sut.assert_reaped().await;
    assert_eq!(
        done.a_leg.messages.entries.len(),
        7,
        "the a-leg: {:?}",
        rows(&done.a_leg.messages.entries)
    );
    let b_rows = rows(&b_leg(&done).messages.entries);
    assert_eq!(b_rows.len(), 6, "the b-leg: {b_rows:?}");
    assert_eq!(b_rows[4].0, Authored);
    assert_eq!(b_rows[4].1, "BYE");
    assert_eq!(b_rows[5], (Received, "BYE", b_rows[4].2, Some(200)));
    // `seq` is per call and monotonic across both legs.
    let mut seqs: Vec<u32> = done
        .a_leg
        .messages
        .entries
        .iter()
        .chain(b_leg(&done).messages.entries.iter())
        .map(|e| e.seq)
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (1..=13).collect::<Vec<u32>>(), "thirteen messages, one seq each");
    assert_eq!(done.message_seq, 13);
    assert_eq!(done.a_leg.messages.dropped, 0);
    assert_eq!(b_leg(&done).messages.dropped, 0);
    // The turn's clock stamps every entry, and the seq keeps their order.
    assert!(done.a_leg.messages.entries.windows(2).all(|w| w[0].at_ms <= w[1].at_ms));

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_prack_round_and_a_reinvite_round_add_their_rows() {
    let h = Harness::new("message-ring-prack-reinvite");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice
        .invite(&bob)
        .with_sdp(OFFER)
        .with_header("Supported", "100rel")
        .through(sut.addr)
        .send()
        .await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(183, "Session Progress").reliable(1).with_sdp(ANSWER).await;
    let p183 = call.expect(183).await;
    let mut prack = call.try_prack(&p183).await.expect("alice PRACKs the reliable 183");
    let mut bob_prack = bob.receive("PRACK").await;
    bob_prack.respond(200, "OK").await;
    prack.expect(200).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    // ── alice re-INVITEs with a new offer ──────────────────────────────────
    let mut reinv = dialog.request(InDialogMethod::Invite, Some(REOFFER)).await;
    let mut bob_uas = bob.receive("INVITE").await;
    bob_uas.respond(200, "OK").with_sdp(REANSWER).await;
    reinv.expect(200).await;
    let reinvite_cseq = dialog.local_cseq();
    dialog.ack(None).await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(5)).await;

    let live = sut.live(&call.call_id(), dialog.local_tag());
    let a = rows(&live.a_leg.messages.entries);
    let prack_cseq = a[3].2;
    assert_eq!(
        a,
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(183)),
            (Received, "PRACK", prack_cseq, None),
            (Relayed, "PRACK", prack_cseq, Some(200)),
            (Relayed, "INVITE", invite_cseq, Some(200)),
            (Received, "ACK", invite_cseq, None),
            (Received, "INVITE", reinvite_cseq, None),
            (Authored, "INVITE", reinvite_cseq, Some(100)),
            (Relayed, "INVITE", reinvite_cseq, Some(200)),
            (Received, "ACK", reinvite_cseq, None),
        ],
        "the a-leg: setup with its PRACK round, then the re-INVITE round"
    );
    let b = rows(&b_leg(&live).messages.entries);
    let b_cseq = b[0].2;
    let b_prack = b[2].2;
    let b_reinvite = b[6].2;
    assert_eq!(
        b,
        vec![
            (Relayed, "INVITE", b_cseq, None),
            (Received, "INVITE", b_cseq, Some(183)),
            (Relayed, "PRACK", b_prack, None),
            (Received, "PRACK", b_prack, Some(200)),
            (Received, "INVITE", b_cseq, Some(200)),
            (Authored, "ACK", b_cseq, None),
            (Relayed, "INVITE", b_reinvite, None),
            (Received, "INVITE", b_reinvite, Some(200)),
            (Authored, "ACK", b_reinvite, None),
        ],
        "the b-leg: the same rounds, as this stack sent them"
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let done = sut.assert_reaped().await;
    assert_eq!(done.a_leg.messages.entries.len(), 13);
    assert_eq!(b_leg(&done).messages.entries.len(), 11);
    assert_eq!(done.message_seq, 24);

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn retransmissions_add_nothing() {
    let h = Harness::new("message-ring-retransmissions");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    // The caller's INVITE again (RFC 3261 §17.2.1): the server transaction
    // replays its 180; the call never sees it.
    let invite = recorded(&h, alice.addr(), sut.addr, b"INVITE ");
    alice.try_send_datagram(&invite, sut.addr).await.unwrap();
    pump(&h, &[&alice]).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    pump(&h, &[&alice, &bob]).await;
    let baseline = sut.live(&call.call_id(), dialog.local_tag());
    assert_eq!(
        baseline.a_leg.messages.entries.len(),
        5,
        "{:?}",
        rows(&baseline.a_leg.messages.entries)
    );
    assert_eq!(b_leg(&baseline).messages.entries.len(), 4);

    // bob's 2xx again, as if the ACK was lost (§13.2.2.4): re-ACKed on the
    // same transaction, recorded nowhere.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    pump(&h, &[&bob]).await;
    // The caller's ACK again (§13.3.1.4): absorbed.
    let ack = recorded(&h, alice.addr(), sut.addr, b"ACK ");
    alice.try_send_datagram(&ack, sut.addr).await.unwrap();
    pump(&h, &[&bob]).await;

    let after = sut.live(&call.call_id(), dialog.local_tag());
    assert_eq!(rows(&after.a_leg.messages.entries), rows(&baseline.a_leg.messages.entries));
    assert_eq!(rows(&b_leg(&after).messages.entries), rows(&b_leg(&baseline).messages.entries));
    assert_eq!(after.message_seq, baseline.message_seq);

    // The caller's BYE again: the non-INVITE server transaction replays its
    // 200 (§17.2.1); nothing reaches the call.
    let mut bye = dialog.bye().await;
    let mut bob_bye = bob.receive("BYE").await;
    bye.expect(200).await;
    let bye_wire = recorded(&h, alice.addr(), sut.addr, b"BYE ");
    alice.try_send_datagram(&bye_wire, sut.addr).await.unwrap();
    pump(&h, &[&alice]).await;
    let terminating = sut.live(&call.call_id(), dialog.local_tag());
    assert_eq!(terminating.a_leg.messages.entries.len(), 7);
    assert_eq!(terminating.message_seq, baseline.message_seq + 3, "BYE, its 200, the relayed BYE");
    bob_bye.respond(200, "OK").await;

    let done = sut.assert_reaped().await;
    assert_eq!(done.message_seq, baseline.message_seq + 4);

    let _report = h.finish().await;
}

/// A CANCEL the transaction layer matches and answers itself (RFC 3261
/// §9.2: 200 to the CANCEL, 487 to the INVITE) reaches the call as its own
/// event; the ring records the three messages, then the CANCEL this stack
/// sends onward and the hop-by-hop ACK its 487 draws (§17.1.1.3).
#[tokio::test(start_paused = true)]
async fn a_cancelled_setup_records_the_layer_answered_cancel() {
    let h = Harness::new("message-ring-cancel");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    let invite_cseq = call.invite_cseq();

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cxl = bob.receive("CANCEL").await;
    bob_cxl.respond(200, "OK").await;
    uas.respond(487, "Request Terminated").await;
    bob.receive("ACK").await;
    h.advance(Duration::from_secs(1)).await;

    let done = sut.assert_reaped().await;
    assert_eq!(
        rows(&done.a_leg.messages.entries),
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(180)),
            (Received, "CANCEL", invite_cseq, None),
            (Authored, "CANCEL", invite_cseq, Some(200)),
            (Authored, "INVITE", invite_cseq, Some(487)),
        ],
        "the a-leg: the CANCEL and the two finals the layer answered it with"
    );
    let b = rows(&b_leg(&done).messages.entries);
    let b_cseq = b[0].2;
    assert_eq!(
        b,
        vec![
            (Relayed, "INVITE", b_cseq, None),
            (Received, "INVITE", b_cseq, Some(180)),
            (Authored, "CANCEL", b_cseq, None),
            (Received, "CANCEL", b_cseq, Some(200)),
            (Received, "INVITE", b_cseq, Some(487)),
            (Authored, "ACK", b_cseq, None),
        ],
        "the b-leg: the CANCEL this stack sent, its answers, the 487's hop ACK"
    );
    // The 487 carries the a-dialog tag the 180 established; the CANCEL,
    // like the INVITE it cancels, none.
    assert_eq!(done.a_leg.messages.entries[3].to_tag, None);
    assert_eq!(done.a_leg.messages.entries[5].to_tag, done.a_leg.messages.entries[2].to_tag);

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_cap_keeps_the_last_entries_and_counts_the_evicted() {
    let h = Harness::new("message-ring-cap");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, CdrConfig { message_ring: 4, captured_headers: Vec::new() }).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    let bye_cseq = dialog.local_cseq();
    assert_eq!(
        rows(&done.a_leg.messages.entries),
        vec![
            (Relayed, "INVITE", invite_cseq, Some(200)),
            (Received, "ACK", invite_cseq, None),
            (Received, "BYE", bye_cseq, None),
            (Authored, "BYE", bye_cseq, Some(200)),
        ],
        "the last four of seven"
    );
    assert_eq!(done.a_leg.messages.dropped, 3);
    assert_eq!(b_leg(&done).messages.entries.len(), 4);
    assert_eq!(b_leg(&done).messages.dropped, 2, "six b-leg messages, four kept");
    assert_eq!(done.message_seq, 13, "the seq counts what was evicted too");
    assert_eq!(done.a_leg.messages.last_seq(), Some(11), "the 200 to the caller's BYE");

    let _report = h.finish().await;
}

#[tokio::test(start_paused = true)]
async fn the_ring_off_records_nothing() {
    let h = Harness::new("message-ring-off");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, CdrConfig::default()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;

    let done = sut.assert_reaped().await;
    assert!(done.a_leg.messages.entries.is_empty());
    assert!(b_leg(&done).messages.entries.is_empty());
    assert_eq!(done.a_leg.messages.dropped, 0);
    assert_eq!(done.message_seq, 0);

    let _report = h.finish().await;
}

/// The liveness probe this stack originates — the in-dialog OPTIONS
/// keepalive — and its answer are not dialog history: a long call keeps its
/// setup rows under a cap the probes alone would have overrun.
#[tokio::test(start_paused = true)]
async fn keepalive_rounds_add_nothing_and_evict_nothing() {
    let h = Harness::new("message-ring-keepalive");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    // Three rounds would add twelve rows across the legs — over this cap.
    let sut = Sut::spawn(&h, CdrConfig { message_ring: 8, captured_headers: Vec::new() }).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;

    for _ in 0..3 {
        h.advance(Duration::from_secs(30)).await;
        alice.receive("OPTIONS").await.respond(200, "OK").await;
        bob.receive("OPTIONS").await.respond(200, "OK").await;
    }
    h.advance(Duration::from_secs(1)).await;

    let live = sut.live(&call.call_id(), dialog.local_tag());
    assert_eq!(
        rows(&live.a_leg.messages.entries),
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(180)),
            (Relayed, "INVITE", invite_cseq, Some(200)),
            (Received, "ACK", invite_cseq, None),
        ],
        "the setup rows survive three keepalive rounds"
    );
    assert_eq!(b_leg(&live).messages.entries.len(), 4);
    assert_eq!(live.a_leg.messages.dropped, 0);
    assert_eq!(b_leg(&live).messages.dropped, 0);
    assert_eq!(live.message_seq, 9);

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    let done = sut.assert_reaped().await;
    assert_eq!(done.a_leg.messages.entries.len(), 7);
    assert_eq!(done.a_leg.messages.dropped, 0);

    let _report = h.finish().await;
}

/// A callee's 200 to a CANCEL carries the INVITE's CSeq number and tag: it is
/// a message of its own, never a copy of the INVITE 2xx the stack ACKed when
/// the answer crossed the CANCEL.
#[tokio::test(start_paused = true)]
async fn a_200_to_cancel_crossing_the_answer_is_recorded() {
    let h = Harness::new("message-ring-cancel-crossing");
    // The crossing under test is bob's deliberate 200 after the CANCEL (RFC
    // 3261 §9.2); the SUT's own output stays under the audit.
    h.waive(
        WaiverScope::rule(
            "no-200-after-cancel",
            "bob deliberately answers 200 after taking the CANCEL — the crossing under test",
        )
        .on_party("bob"),
    );
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;

    let mut cxl = call.cancel().await;
    cxl.expect(200).await;
    call.expect(487).await;
    let mut bob_cancel = bob.receive("CANCEL").await;
    // Crossing: bob answers the INVITE before the CANCEL takes effect, then
    // answers the CANCEL; the stack ACKs the answer and BYEs the leg.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    bob_cancel.respond(200, "OK").await;
    bob.receive("ACK").await;
    bob.receive("BYE").await.respond(200, "OK").await;
    h.advance(Duration::from_secs(1)).await;

    let done = sut.assert_reaped().await;
    let b = rows(&b_leg(&done).messages.entries);
    let b_cseq = b[0].2;
    assert!(
        b.contains(&(Received, "CANCEL", b_cseq, Some(200))),
        "the 200 to the CANCEL is a row of its own: {b:?}"
    );
    assert!(b.contains(&(Received, "INVITE", b_cseq, Some(200))), "{b:?}");
    assert_eq!(b.iter().filter(|r| r.0 == Received && r.3 == Some(200)).count(), 3, "{b:?}");
    assert_eq!(b.last().map(|r| (r.0, r.1, r.3)), Some((Received, "BYE", Some(200))), "{b:?}");

    let _report = h.finish().await;
}

/// A delayed-offer answer: the callee repeats its 200 until the caller's ACK
/// supplies the answer (RFC 3261 §13.3.1.4); every copy is the 2xx the
/// dialog took, one row.
#[tokio::test(start_paused = true)]
async fn a_repeated_delayed_offer_answer_is_one_row() {
    let h = Harness::new("message-ring-delayed-offer");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn(&h, ring_on()).await;

    let mut call = alice.invite(&bob).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    // Two more copies before alice's ACK.
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    pump(&h, &[&alice]).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    pump(&h, &[&alice]).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack_with(Some(OFFER)).await;
    bob.receive("ACK").await;
    pump(&h, &[&alice, &bob]).await;

    let live = sut.live(&call.call_id(), dialog.local_tag());
    let b = rows(&b_leg(&live).messages.entries);
    let b_cseq = b[0].2;
    assert_eq!(
        b,
        vec![
            (Relayed, "INVITE", b_cseq, None),
            (Received, "INVITE", b_cseq, Some(180)),
            (Received, "INVITE", b_cseq, Some(200)),
            (Relayed, "ACK", b_cseq, None),
        ],
        "one 200, and the caller's ACK relayed with its answer"
    );
    assert_eq!(
        rows(&live.a_leg.messages.entries),
        vec![
            (Received, "INVITE", invite_cseq, None),
            (Authored, "INVITE", invite_cseq, Some(100)),
            (Relayed, "INVITE", invite_cseq, Some(180)),
            (Relayed, "INVITE", invite_cseq, Some(200)),
            (Received, "ACK", invite_cseq, None),
        ]
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    sut.assert_reaped().await;

    let _report = h.finish().await;
}

/// A caller's first ACK arriving after the §13.3.1.4 ladder gave up on it is
/// still the first: the ring records it, an obligation or not.
#[tokio::test(start_paused = true)]
async fn a_late_first_ack_after_the_give_up_is_recorded() {
    let h = Harness::new("message-ring-late-ack");
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    let sut = Sut::spawn_tuned(&h, ring_on(), |c| c.ack_timeout_sec = 6).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    bob.receive("ACK").await;

    // Alice holds her ACK past the give-up: the stack BYEs both legs.
    h.advance(Duration::from_secs(8)).await;
    alice.drain().await;
    let mut alice_bye = alice.receive("BYE").await;
    let mut bob_bye = bob.receive("BYE").await;
    // Her ACK lands now, on a terminating call.
    let dialog = call.ack().await;
    pump(&h, &[&alice, &bob]).await;
    let live = sut.live(&call.call_id(), dialog.local_tag());
    let a = rows(&live.a_leg.messages.entries);
    assert!(
        a.contains(&(Received, "ACK", invite_cseq, None)),
        "the late ACK is the first of its 2xx: {a:?}"
    );
    assert_eq!(a.iter().filter(|r| r.1 == "ACK").count(), 1, "{a:?}");

    alice_bye.respond(200, "OK").await;
    bob_bye.respond(200, "OK").await;
    sut.assert_reaped().await;

    let _report = h.finish().await;
}

/// A CANCEL arriving once the INVITE transaction is gone matches nothing and
/// draws the router's 481 (RFC 3261 §9.2): the call is live, so both are
/// rows of its a-leg.
#[tokio::test(start_paused = true)]
async fn a_stray_cancel_and_its_481_are_recorded() {
    let h = Harness::new("message-ring-stray-cancel");
    // The stray CANCEL is alice's deliberate one (RFC 3261 §9.1 left her
    // nothing to cancel); the SUT's 481 stays under the audit.
    h.waive(
        WaiverScope::rule(
            "no-cancel-after-final",
            "alice deliberately CANCELs a completed INVITE — the stray CANCEL under test",
        )
        .on_party("alice"),
    );
    let alice = h.agent("alice", ALICE).await;
    let bob = h.agent("bob", BOB).await;
    // No keepalive round inside the wait for Timer L (RFC 6026 §7.1, 32 s).
    let sut = Sut::spawn_tuned(&h, ring_on(), |c| c.keepalive_interval_sec = 120).await;

    let mut call = alice.invite(&bob).with_sdp(OFFER).through(sut.addr).send().await;
    let mut uas = bob.receive("INVITE").await;
    uas.respond(180, "Ringing").await;
    call.expect(180).await;
    uas.respond(200, "OK").with_sdp(ANSWER).await;
    call.expect(200).await;
    let invite_cseq = call.invite_cseq();
    let mut dialog = call.ack().await;
    bob.receive("ACK").await;
    // Past Timer L (RFC 6026 §7.1): the INVITE server transaction is gone.
    h.advance(Duration::from_secs(40)).await;

    let invite = recorded(&h, alice.addr(), sut.addr, b"INVITE ");
    let cancel = cancel_of(&invite);
    alice.try_send_datagram(&cancel, sut.addr).await.unwrap();
    pump(&h, &[&alice, &bob]).await;

    let live = sut.live(&call.call_id(), dialog.local_tag());
    let a = rows(&live.a_leg.messages.entries);
    assert_eq!(
        a[5..],
        [(Received, "CANCEL", invite_cseq, None), (Authored, "CANCEL", invite_cseq, Some(481))],
        "{a:?}"
    );

    let mut bye = dialog.bye().await;
    bob.receive("BYE").await.respond(200, "OK").await;
    bye.expect(200).await;
    sut.assert_reaped().await;

    let _report = h.finish().await;
}

/// The CANCEL of a recorded INVITE datagram (RFC 3261 §9.1): the same
/// Request-URI, Via, From, To, Call-ID and CSeq number, no body.
fn cancel_of(invite: &[u8]) -> Vec<u8> {
    let text = std::str::from_utf8(invite).expect("the INVITE is text");
    let head = text.split("\r\n\r\n").next().unwrap_or(text);
    let mut out = String::new();
    for (i, line) in head.split("\r\n").enumerate() {
        let lower = line.to_ascii_lowercase();
        if i == 0 {
            out.push_str(&line.replacen("INVITE ", "CANCEL ", 1));
        } else if lower.starts_with("cseq:") {
            out.push_str(&line.replace("INVITE", "CANCEL"));
        } else if lower.starts_with("content-") || lower.starts_with("contact:") {
            continue;
        } else {
            out.push_str(line);
        }
        out.push_str("\r\n");
    }
    out.push_str("Content-Length: 0\r\n\r\n");
    out.into_bytes()
}
