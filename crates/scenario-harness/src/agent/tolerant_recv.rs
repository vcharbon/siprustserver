//! Method-filtered receive policies on [`Agent`]: *tolerate* (answer 200 OK
//! and keep waiting), *absorb* (drop silently and keep waiting), and the two
//! bounded drain windows — the strict transfer-lane
//! [`drain_expecting`](Agent::drain_expecting) and the failed-call
//! [`release_failed_call`](Agent::release_failed_call). The plain asserting
//! receive lives in [`super::ua`].

use std::time::Duration;

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::addressing::top_via_addr;
use super::server_txn::ServerTxn;
use super::step::StepError;
use super::txn_view::TxnVerdict;
use super::Agent;

/// The stateless answer a UA with **no remaining dialog state** owes an
/// inbound request during a failed-call release (see
/// [`Agent::release_failed_call`]). `None` ⇒ absorb, no response (an ACK).
/// `has_to_tag` distinguishes an in-dialog request (a keepalive OPTIONS, an
/// INFO) from an out-of-dialog probe.
fn release_verdict(method: &str, has_to_tag: bool) -> Option<(u16, &'static str)> {
    match method {
        // An ACK takes no response, ever.
        "ACK" => None,
        // Complete the teardown transaction the SUT is driving.
        "BYE" | "CANCEL" => Some((200, "OK")),
        // A bodyless 200 here would CONFIRM a leg the teardown is killing (a
        // queued Timer-A INVITE retransmit) or answer an offer with no SDP
        // (RFC 3264 §5); 481 releases it honestly.
        "INVITE" | "UPDATE" => Some((481, "Call/Transaction Does Not Exist")),
        // Any other in-dialog request has no dialog here (RFC 3261 §12.2.2);
        // an out-of-dialog probe gets a plain 200.
        _ if has_to_tag => Some((481, "Call/Transaction Does Not Exist")),
        _ => Some((200, "OK")),
    }
}

impl Agent {
    /// **Failed-call release** — the load driver's teardown pump for a call
    /// that FAILED: for up to `window`, answer whatever the SUT still sends so
    /// it can reap its legs promptly instead of waiting out retransmit timers,
    /// as a UA that no longer has the call would:
    /// - **ACK** — absorbed, never answered (an ACK takes no response; a
    ///   matching §17.1.1.3 obligation is still claimed);
    /// - **BYE / CANCEL** — `200 OK` (completes the teardown transaction the
    ///   SUT is driving);
    /// - **INVITE / UPDATE** — `481 Call/Transaction Does Not Exist`: a
    ///   bodyless `200` here would CONFIRM a leg the teardown is killing (a
    ///   queued Timer-A INVITE retransmit answered 200 = a zombie crossing-2xx
    ///   the SUT must then ACK+BYE away), and an offer-carrying re-INVITE may
    ///   never be answered `200` without an SDP answer (RFC 3264 §5);
    /// - **any other in-dialog request** (`To` tag present: INFO, NOTIFY,
    ///   OPTIONS keepalive, …) — `481`, the honest stateless answer of a UA
    ///   with no remaining dialog state (RFC 3261 §12.2.2);
    /// - **any other out-of-dialog request** — `200 OK` (a plain probe).
    ///
    /// Never panics (sends are best-effort). For the transfer-lane drain on a
    /// SUCCESSFUL call use [`drain_expecting`](Agent::drain_expecting), which
    /// asserts the peer still behaves as a live dialog member.
    pub async fn release_failed_call(&self, window: Duration) {
        let deadline = tokio::time::Instant::now() + window;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            let pkt = match tokio::time::timeout(remaining, self.ep.recv()).await {
                Ok(Some(pkt)) => pkt,
                _ => return, // timed out or queue closed
            };
            let Ok(SipMessage::Request(r)) = CustomParser::new().parse(&pkt.raw) else {
                continue; // responses / unparseable noise: nothing to answer
            };
            let Some((status, reason)) = release_verdict(r.method.as_str(), r.to.tag.is_some())
            else {
                // ACK: absorbed, never answered — but still claim any matching
                // §17.1.1.3 obligation so a later `expect_ack` sees it settled.
                self.ack_obligation_claims(&r);
                continue;
            };
            let resp = generate_response(&r, status, reason, &GenerateResponseOpts::default());
            let dst = top_via_addr(&r).unwrap_or(self.addr);
            let _ = self.try_send(&SipMessage::Response(resp), dst).await;
        }
    }

    /// **Strict transfer-lane drain** — the post-flow straggler window for a
    /// SUCCESSFUL multi-leg call (e.g. the SUT's relayed BYE toward a callee
    /// leg landing just after the scenario's own drain closed): for up to
    /// `window`, answer each request whose method is in `answer_200` with a
    /// proper `200 OK` (through [`ServerTxn`], so the response is the same
    /// compliant one a live UA sends), absorb ACKs (claiming any §17.1.1.3
    /// obligation), and **panic on anything else** — this UA is still a live
    /// dialog member, so unexpected traffic is a test failure, never something
    /// to blind-answer. Returns the number of requests answered (possibly 0 —
    /// whether a straggler exists is timing-dependent; asserting that a
    /// specific message ARRIVES belongs to the scenario body, not this window).
    pub async fn drain_expecting(&self, window: Duration, answer_200: &[&str]) -> usize {
        let deadline = tokio::time::Instant::now() + window;
        let mut answered = 0;
        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return answered;
            }
            let pkt = match tokio::time::timeout(remaining, self.ep.recv()).await {
                Ok(Some(pkt)) => pkt,
                _ => return answered, // window elapsed or queue closed
            };
            let msg = CustomParser::new()
                .parse(&pkt.raw)
                .unwrap_or_else(|e| panic!("{} drained an unparseable datagram: {e}", self.name));
            let r = match msg {
                SipMessage::Request(r) => r,
                SipMessage::Response(resp) => panic!(
                    "{} drained an unexpected {} {} response (expecting only {answer_200:?})",
                    self.name, resp.status, resp.reason
                ),
            };
            if r.method.as_str() == "ACK" {
                self.ack_obligation_claims(&r);
                continue;
            }
            if answer_200.iter().any(|m| r.method == *m) {
                let mut txn = ServerTxn::from_request(self.clone(), r);
                txn.respond(200, "OK").send().await;
                answered += 1;
                continue;
            }
            panic!(
                "{} drained an unexpected {} request (expecting only {answer_200:?})",
                self.name, r.method
            );
        }
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
                continue; // txn-owned §17.1.1.3 hop ACK
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

    /// **Blocking**, fallible, tolerant receive — the assertable load-lane
    /// primitive: wait (up to `recv_timeout` per datagram) until a `method`
    /// request arrives, answering every interleaved `tolerate` request
    /// `200 OK` along the way, and return the matched [`ServerTxn`] **plus
    /// everything that was absorbed** so the body can ASSERT the interleaved
    /// traffic instead of blind-draining it.
    ///
    /// Contract details that make this assertable:
    /// - a missing `method` request is a [`StepError::Timeout`] — a lost BYE
    ///   cannot masquerade as success;
    /// - an interleaved **ACK** is absorbed + collected WITHOUT a response (an
    ///   ACK completes a transaction and must never be answered), whether or
    ///   not it is listed in `tolerate`;
    /// - a tolerated **offer-carrying INVITE/UPDATE** (a realign re-INVITE) is
    ///   answered `200` **with an SDP answer** — RFC 3264 §5 / RFC 3261
    ///   §13.3.1.1 forbid a bodyless 200 to an offer;
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

    /// Like [`receive`](Agent::receive), but first drains (and 200-OKs) any
    /// requests whose method is in `tolerate`. Under a paused clock an advance
    /// that crosses a timer deadline emits a request whose 2xx round-trip races
    /// the txn-layer retransmit, so several identical copies queue before the
    /// awaited message (CLAUDE.md: tolerate retransmits, don't relax the
    /// assertion). Returns the first request matching `method`.
    pub async fn receive_tolerating(&self, method: &str, tolerate: &[&str]) -> ServerTxn {
        loop {
            let msg = self.recv().await;
            match msg {
                SipMessage::Request(r) => {
                    if r.method == method {
                        return ServerTxn::from_request(self.clone(), r);
                    }
                    if self.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK
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
    /// The §17.2 receive view ([`super::txn_view::TxnView`]) already absorbs
    /// **byte-identical retransmissions** automatically, so most flows need a
    /// plain [`receive`](Agent::receive). This remains for (a)
    /// [`wire_view`](Agent::wire_view) agents, and (b) absorbing *distinct*
    /// same-method requests — which it matches by METHOD NAME ONLY, so a
    /// genuinely unexpected request of that method is masked too. Prefer a
    /// plain [`receive`](Agent::receive) first.
    ///
    /// The load-bearing difference from [`receive_tolerating`](Agent::receive_tolerating)
    /// is that the absorbed request is **not** `200 OK`'d: a 200 would ANSWER it.
    /// The intended case is a b-leg INVITE the callee is deliberately leaving
    /// unanswered (a **silent callee**, no `>=180`) while the UAC's Timer A keeps
    /// retransmitting the INVITE hop-by-hop through a front proxy that absorbs the
    /// callee's bare `100 Trying` (RFC 3261 §16.7) — so the UAC never stops its
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
                        continue; // txn-owned §17.1.1.3 hop ACK
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
}

#[cfg(test)]
mod release_verdict_tests {
    //! The failed-call release decision table ([`release_verdict`]): a UA with
    //! no remaining dialog state answers teardown traffic honestly. The pure
    //! decision is tested here; the socket drive (Via-routing, ACK obligation
    //! claim) is exercised through the load lane's soak scenarios.

    use super::release_verdict;

    #[test]
    fn ack_is_absorbed() {
        // An ACK completes a transaction and takes no response, in or out of
        // dialog.
        assert_eq!(release_verdict("ACK", true), None);
        assert_eq!(release_verdict("ACK", false), None);
    }

    #[test]
    fn teardown_transactions_get_200() {
        for m in ["BYE", "CANCEL"] {
            assert_eq!(release_verdict(m, true), Some((200, "OK")), "{m}");
        }
    }

    #[test]
    fn invite_and_update_get_481_never_200() {
        // The load-bearing regression guard: a bodyless 200 to a queued INVITE
        // retransmit would confirm a zombie leg the teardown is killing.
        for m in ["INVITE", "UPDATE"] {
            assert_eq!(
                release_verdict(m, false),
                Some((481, "Call/Transaction Does Not Exist")),
                "{m} (out of dialog)"
            );
            assert_eq!(
                release_verdict(m, true),
                Some((481, "Call/Transaction Does Not Exist")),
                "{m} (in dialog)"
            );
        }
    }

    #[test]
    fn in_dialog_requests_get_481_out_of_dialog_probes_get_200() {
        // An in-dialog INFO/NOTIFY/OPTIONS keepalive (To-tag present) has no
        // dialog here → 481; a bare out-of-dialog OPTIONS probe → 200.
        for m in ["INFO", "NOTIFY", "OPTIONS", "MESSAGE"] {
            assert_eq!(
                release_verdict(m, true),
                Some((481, "Call/Transaction Does Not Exist")),
                "in-dialog {m}"
            );
            assert_eq!(release_verdict(m, false), Some((200, "OK")), "out-of-dialog {m}");
        }
    }
}
