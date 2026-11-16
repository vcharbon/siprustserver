//! The load lane's wait for a [`ClientInvite`]'s final: provisionals
//! collected rather than expected one by one, responses to other transactions
//! sorted as every expect sorts them (RFC 3261 §17.1.3).

use sip_message::{SipMessage, SipResponse};

use super::super::client_txn::{awaited_response, put_back, step_over_or_set_aside, Awaited};
use super::super::step::StepError;
use super::ClientInvite;

impl ClientInvite {
    /// SIPp-`optional` semantics for the load lane: wait for the FINAL
    /// response `status`, absorbing — and collecting — any interleaved
    /// provisional (1xx), instead of erroring on it the way
    /// [`try_expect`](Self::try_expect) does. A multi-leg shape legitimately
    /// sees a relay-timing-dependent number of provisionals (the reroute
    /// caller gets one or two 180s depending on when the alternate leg rings),
    /// so a body says `try_expect_final(200)` and asserts the collected 1xx
    /// list only where the count IS deterministic. Every absorbed provisional
    /// still feeds the dialog bookkeeping (early To-tag, Contact, route set),
    /// exactly as a sequence of `try_expect(18x)` calls would; the final
    /// overrides per §13.2.2.4. A non-matching FINAL is still a
    /// [`StepError::WrongStatus`].
    ///
    /// NOTE: under `--auto-retransmit` the mux absorbs a REPEATED
    /// byte-identical provisional as a retransmission before any body sees it
    /// — see the retransmit-engine notes in `loadgen::mux` (`CallTxns`); a
    /// "ring again" assertion belongs to the functional/e2e surface, not here.
    pub async fn try_expect_final(
        &mut self,
        status: u16,
    ) -> Result<(SipResponse, Vec<SipResponse>), StepError> {
        let mut aside = Vec::new();
        let read = self.read_final(status, &mut aside).await;
        put_back(&self.agent, aside);
        read
    }

    /// The loop of [`try_expect_final`](Self::try_expect_final), gathering
    /// into `aside` the responses to other transactions it sets aside.
    async fn read_final(
        &mut self,
        status: u16,
        aside: &mut Vec<SipResponse>,
    ) -> Result<(SipResponse, Vec<SipResponse>), StepError> {
        let mut provisionals = Vec::new();
        loop {
            let next = self
                .agent
                .try_recv()
                .await
                .map_err(|e| e.awaiting(|| awaited_response(status, &self.original_invite)));
            match next? {
                SipMessage::Response(r) if r.status() == 100 => continue,
                SipMessage::Response(r) if !Awaited::Invite(self.ack_ctx()).owns(&r) => {
                    step_over_or_set_aside(&self.agent, r, aside)
                }
                SipMessage::Response(r) if r.status() < 200 => {
                    self.agent.txn.note_read(&r);
                    self.learn_from_response(&r);
                    provisionals.push(r);
                }
                SipMessage::Response(r) => {
                    self.agent.txn.note_read(&r);
                    // §17.1.1.3 txn-layer auto-ACK — matching or not, a non-2xx
                    // final to THIS INVITE completes its client transaction.
                    self.ack_ctx().ack_non_2xx(&r).await?;
                    if r.status() != status {
                        return Err(StepError::WrongStatus {
                            who: self.agent.name.clone(),
                            expected: status,
                            got: r.status(),
                            reason: r.reason().to_string(),
                        });
                    }
                    self.learn_from_response(&r);
                    return Ok((r, provisionals));
                }
                SipMessage::Request(r) => {
                    if self.agent.ack_obligation_claims(&r) {
                        continue; // txn-owned §17.1.1.3 hop ACK
                    }
                    return Err(StepError::UnexpectedKind {
                        who: self.agent.name.clone(),
                        detail: format!("got a {} request, expected a {status} final", r.method()),
                    });
                }
            }
        }
    }
}
