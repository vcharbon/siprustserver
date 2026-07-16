//! The transaction-layer receive view: RFC 3261 §17.2 once-and-only-once
//! dedup ([`TxnView`]) and §17.1.1.3 UAS-side ACK obligations
//! ([`AckObligations`]). Both sit below the test-facing receive API on
//! [`Agent`](super::Agent), shared across clones of one logical UA.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use sip_message::SipMessage;

use super::addressing::top_via_branch;

/// RFC 3261 §17.2 **once-and-only-once receive view** — the transaction-layer
/// dedup below the test-facing receive API. Without it a body would re-absorb
/// Timer A/E retransmissions by hand with method-name lists, which
/// over-approximate "a retransmission of the request I already saw" into "any
/// request of this method" — a genuinely new request could be silently
/// swallowed as noise.
///
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
///   legitimate B2BUA relay observable ("ring again", one a-leg early dialog),
///   not something the harness may hide.
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
/// The load-lane [`loadbind::AgentBinder`](crate::loadbind::AgentBinder)
/// constructs its agents in **wire view** ([`TxnView::wire`]): the mux already
/// dedups ahead of the agent there, and double-dedup would silently change
/// load semantics.
pub(crate) struct TxnView {
    /// Raw-surface opt-out ([`Agent::wire_view`](super::Agent::wire_view));
    /// shared by every clone of the UA so the whole logical endpoint drops to
    /// the wire together.
    pub(super) wire: AtomicBool,
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
pub(super) enum TxnVerdict {
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

    pub(super) fn verdict(&self, raw: &[u8], msg: &SipMessage) -> TxnVerdict {
        if self.wire.load(Ordering::Relaxed) {
            return TxnVerdict::Surface;
        }
        match msg {
            SipMessage::Request(r) => {
                // Unkeyable (no top-Via branch / pre-RFC3261 cookie): surface —
                // graceful degradation to the raw behaviour.
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

/// §17.1.1.3 UAS-side ACK obligations — the receive-side mirror of the client
/// auto-ACK: when a body answers an INVITE with a **non-2xx final** through
/// [`ServerTxn`](super::ServerTxn)/[`Respond`](super::Respond), the
/// transaction layer — not the body — owns the arriving hop ACK.
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
///   still **fulfils** the obligation —
///   [`ServerTxn::expect_ack`](super::ServerTxn::expect_ack) then returns
///   immediately;
/// - a matching ACK nothing ever pulls is still recorded at delivery, so the
///   gating wire rule discharges at `finish()` — that rule IS the settle gate;
///   no duplicate receive-side gate exists.
///
/// Shared across clones of one logical UA (like [`TxnView`]); works identically
/// in the load lane's wire view (claiming is independent of dedup).
#[derive(Default)]
pub(crate) struct AckObligations {
    /// `(Call-ID, INVITE top-Via branch)` → the hop ACK has been sighted.
    pending: Mutex<HashMap<(String, String), bool>>,
    /// Wakes an [`ServerTxn::expect_ack`](super::ServerTxn::expect_ack) parked
    /// on fulfilment.
    notify: tokio::sync::Notify,
}

impl AckObligations {
    /// Open (or refresh — a retransmitted final re-arms the same key without
    /// clearing a sighting) the obligation for one rejected INVITE transaction.
    pub(super) fn arm(&self, call_id: String, branch: String) {
        self.pending.lock().unwrap().entry((call_id, branch)).or_insert(false);
    }

    /// Record an ACK sighting. Returns `true` iff the key belongs to an armed
    /// obligation (fulfilled now or previously) — the caller may absorb it.
    pub(super) fn note_ack(&self, call_id: &str, branch: &str) -> bool {
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

    pub(super) fn is_fulfilled(&self, call_id: &str, branch: &str) -> bool {
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
    pub(super) async fn fulfilled(&self, call_id: &str, branch: &str) {
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

#[cfg(test)]
mod verdict_tests {
    //! The §17.2 once-and-only-once receive view: pure verdict tests pin the
    //! keying/byte-identity semantics. The end-to-end contract (a Timer-A
    //! style duplicate never surfaces; `wire_view()` restores the raw surface)
    //! is pinned in [`super::super::tests`].

    use sip_message::parser::custom::CustomParser;
    use sip_message::SipParser;

    use super::*;

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
        // A byte-identical second 180 is the ring-again observable: the
        // functional lane is exactly where it must stay visible.
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
}
