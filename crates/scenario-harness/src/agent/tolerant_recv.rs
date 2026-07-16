//! Method-filtered receive policies on [`Agent`]: *tolerate* (answer 200 OK
//! and keep waiting) and *absorb* (drop silently and keep waiting) — for
//! flows where retransmits or interleaved traffic race the message under
//! test. The plain asserting receive lives in [`super::ua`].

use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

use super::server_txn::ServerTxn;
use super::step::StepError;
use super::txn_view::TxnVerdict;
use super::Agent;

impl Agent {
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

    /// **Blocking**, fallible, tolerant receive — the load-lane primitive that
    /// replaces `quiesce`-as-choreography: wait (up to `recv_timeout` per
    /// datagram) until a `method` request arrives, answering every interleaved
    /// `tolerate` request `200 OK` along the way, and return the matched
    /// [`ServerTxn`] **plus everything that was absorbed** so the body can
    /// ASSERT the interleaved traffic instead of blind-draining it.
    ///
    /// Contract details that make this assertable where `quiesce` is not:
    /// - a missing `method` request is a [`StepError::Timeout`] — a lost BYE
    ///   can no longer masquerade as success;
    /// - an interleaved **ACK** is absorbed + collected WITHOUT a response (an
    ///   ACK completes a transaction and must never be answered), whether or
    ///   not it is listed in `tolerate`;
    /// - a tolerated **offer-carrying INVITE/UPDATE** (a realign re-INVITE) is
    ///   answered `200` **with an SDP answer** — RFC 3264 §5 / RFC 3261
    ///   §13.3.1.1 forbid the bodyless 200 that `quiesce`'s bare drain sends;
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
    /// callee's bare `100 Trying` (RFC 3261 §16.7) — so the UAC never quiesces its
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
