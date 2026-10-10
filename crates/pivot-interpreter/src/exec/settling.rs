//! The settle phase, and every datagram arriving inside it.

use std::collections::BTreeMap;
use std::time::Duration;

use pivot_schema::bundle::{Dir, Failure};
use scenario_harness::absorption::Owner;
use sip_message::SipMessage;
use tokio::time::Instant;

use super::flow::Wake;
use super::stall::Stall;
use super::unscripted::{OWED_BYE_FINAL, OWED_FINAL_ACK};
use super::Runner;
use crate::close::Owed;
use crate::gate::Inbound;
use crate::resolve::Resolver;
use crate::settle::{self, Sut};

impl<'a, 'p> Runner<'a, 'p> {
    /// One datagram arriving during the settle window.
    ///
    /// Absorption and `background` behave exactly as they do mid-flow — a
    /// provable repeat is absorbed and noted, a policy's traffic is answered —
    /// and everything else is RECORDED. It is a failure only when the flow had
    /// succeeded: then nothing was left to arrive, and something did. The
    /// finding does not silence the endpoint: an act the RFC makes its own
    /// whatever the document scripts — the `200` to a BYE on the dialog it
    /// holds (RFC 3261 §15.1.2), the final a PRACK draws (RFC 3262 §3) — is
    /// answered here as it is mid-flow, so the peer's transaction ends instead
    /// of retransmitting to its timer while the run waits on the call to end.
    /// The ACK a non-2xx final this leg sent is owed (§17.1.1.3) is what the
    /// settle floor waits for: absorbed, never a late arrival.
    pub(super) async fn record_during_settle(
        &mut self,
        actor: &str,
        message: SipMessage,
        flow_ok: bool,
    ) {
        let inbound = Inbound::of(&message);
        let bytes: Vec<u8> = match &message {
            SipMessage::Request(r) => r.image().to_vec(),
            SipMessage::Response(r) => r.image().to_vec(),
        };
        let leg =
            self.leg_by_call_id(&inbound.call_id).unwrap_or_else(|| "unattributed".to_string());

        let sighting = self.absorption.sight(&bytes, &message);
        let repeat = sighting.repeat;
        if sighting.owner == Owner::TxnDuplicate {
            self.repeats.note(&leg, &bytes, self.now_us());
            self.record_arrival(
                &leg,
                bytes,
                None,
                Some("absorbed during settle: byte-identical retransmission"),
                repeat,
            );
            return;
        }
        // A declared ladder's tail legitimately lands in the settle window: it is
        // counted and recorded there exactly as it is mid-flow, and it is not the
        // late arrival that fails a completed flow.
        if let Some(step) = self.repeats.note(&leg, &bytes, self.now_us()) {
            let note =
                format!("retransmission of the datagram step {step:?} matched, during settle");
            self.instance.recording().push_repeat(
                &leg,
                Dir::In,
                self.now_us(),
                bytes,
                None,
                Some(&note),
            );
            self.draw_for_repeat(&message).await;
            return;
        }
        if let Some((leg, verdict)) = self.dialog_verdict(actor, &message) {
            if verdict.kind.refusal().is_some() {
                self.refuse_foreign_dialog(actor, &leg, &message, bytes, repeat).await;
                return;
            }
            self.report_dialog_verdict(&leg, &message, &verdict);
        }
        if let Some(method) = &inbound.method {
            if let Some((index, policy)) = self.background_policy(
                actor,
                method,
                self.leg_by_call_id(&inbound.call_id).as_deref(),
            ) {
                self.record_arrival(
                    &leg,
                    bytes,
                    None,
                    Some("background policy, during settle"),
                    repeat,
                );
                self.instance.note_background_answered(index);
                self.answer_background(actor, &leg, &message, policy.status).await;
                return;
            }
        }
        if self.owed_bye_final(&leg, &inbound) {
            self.record_arrival(&leg, bytes, None, Some(OWED_BYE_FINAL), repeat);
            return;
        }
        if self.owed_final_ack(&leg, &inbound) {
            self.record_arrival(&leg, bytes, None, Some(OWED_FINAL_ACK), repeat);
            return;
        }
        // A leg whose script ENDED still has a UA behind it, answering out of
        // the same dialog state the flow used. So the stack learns what its leg
        // takes here too, past the same absorbed final `deliver` never learns
        // — a request it never took is one it cannot answer (§11.2), whether
        // the generic close or this window answers it.
        if let Some(stack) = self.stacks.get_mut(&leg) {
            match &message {
                SipMessage::Request(r) => stack.learn_request(r),
                SipMessage::Response(r) => stack.learn_response(r),
            }
        }
        self.record_arrival(
            &leg,
            bytes,
            None,
            Some(if flow_ok {
                "arrived after the flow completed"
            } else if self.abandoned.is_some() {
                "arrived after the script ended, during the generic close"
            } else {
                "arrived during settle, after the flow had already failed"
            }),
            repeat,
        );
        // One diagnosis, not one per datagram: a system tearing down after a
        // completed flow can send several, and they are all the same finding.
        if flow_ok && !self.late_datagram_reported {
            self.late_datagram_reported = true;
            self.instance
                .fail(Failure::DatagramAfterFlow { leg: leg.clone(), arrived: inbound.arrived() });
        }
        // The transaction obligation the arrival leaves is discharged where it
        // is mid-flow; an abandoned script's is the generic close's next turn,
        // which keeps its own order across everything the leg holds open —
        // CANCEL first, then PRACK, then the oldest.
        if self.abandoned.is_none() {
            self.answer_unscripted(&leg, &message).await;
        }
    }

    /// The settle phase (§10). Failing to settle is always run failure.
    ///
    /// The sockets KEEP BEING PUMPED here, and everything that arrives is
    /// recorded: the settle window is exactly where a teardown the system owes
    /// shows up, and a run that stopped listening cannot corroborate its own
    /// "one active call" from the wire. What a datagram MEANS in this window
    /// depends on `flow_ok`: after a flow that succeeded it is a late arrival
    /// nothing scripted and it FAILS the run; after a flow that already failed
    /// it is the aftermath of that failure and is recorded with a note, since
    /// stacking a second diagnosis on the first says nothing new.
    pub(super) async fn settle(
        &mut self,
        sut: &dyn Sut,
        budget_ms: u64,
        flow_ok: bool,
    ) -> Option<u64> {
        let post = self.instance.plan().document().postconditions.clone();
        let deadline = Instant::now() + Duration::from_millis(budget_ms);
        let mut timed_out = false;
        let mut stall = Stall::new(self.wall_ceiling());
        loop {
            if let Some(failure) = stall.check("settle", || {
                let done = self.instance.cursor().is_done() || self.abandoned.is_some();
                let mut open = settle::open_reasons(done, sut, post.as_ref());
                open.extend(close_reasons(&self.close_obligations()));
                open.extend(settle::floor(&self.instance.recording(), self.now_us()).reasons());
                open
            }) {
                self.instance.fail(failure);
                return None;
            }
            // A script a declared divergence ENDED is a flow that is over by
            // ruling (§11.2), and what is still open on it is the close's.
            let flow_done = self.instance.cursor().is_done() || self.abandoned.is_some();
            let owed = self.close_obligations();
            // A non-2xx INVITE final a leg sent is a server transaction the
            // system's ACK ends (RFC 3261 §17.2.1); past Timer H it is gone
            // and the ACK never came, which is the system's failure to state.
            let floor = settle::floor(&self.instance.recording(), self.now_us());
            for expired in &floor.expired {
                if self.timer_h_reported.insert((expired.leg.clone(), expired.owed.cseq)) {
                    self.instance.fail(Failure::FinalUnacknowledged {
                        leg: expired.leg.clone(),
                        status: expired.owed.status,
                        cseq: expired.owed.cseq,
                    });
                }
            }
            // A rung still owed keeps the run open: the flow ending does not
            // end a transaction's own timer, and `drive` counts the ladders
            // only after settle for exactly that reason.
            if owed.is_empty()
                && self.pending_repeats.is_empty()
                && floor.held.is_empty()
                && settle::is_settled(flow_done, sut, post.as_ref())
            {
                break;
            }
            if Instant::now() >= deadline {
                let mut open = settle::open_reasons(flow_done, sut, post.as_ref());
                open.extend(close_reasons(&owed));
                open.extend(floor.reasons());
                self.instance.fail(Failure::SettleTimedOut { budget_ms, open });
                timed_out = true;
                break;
            }
            // A ladder outliving the flow keeps its own clock here.
            if !self.drain_repeats().await {
                break;
            }
            // Whatever the close still owes goes out BEFORE the loop waits again,
            // so the answer to it is what the next turn drains.
            self.close_turn(owed).await;
            // Poll in small steps so the settle condition is re-read as the
            // system tears down, and drain whatever arrives in between.
            let mut wake = deadline.min(Instant::now() + Duration::from_millis(20));
            if let Some(rung) = self.next_repeat() {
                wake = wake.min(rung);
            }
            match self.wait(wake).await {
                Wake::Timer => {}
                Wake::Message { actor, message } => {
                    self.record_during_settle(&actor, *message, flow_ok).await;
                }
                Wake::Closed { actor, detail } => {
                    self.instance.fail(Failure::TransportClosed { actor, detail });
                    break;
                }
            }
        }
        // A `background` count is a fact about the PERIOD the run covered (§5.1),
        // and the period is over whether or not the system settled. So the
        // counters are read either way: a run that ran out of budget still says
        // what its endpoints heard, and an absence counter still catches the
        // traffic that should never have reached them.
        for failure in self.instance.background().settle_failures() {
            self.instance.fail(failure);
        }
        if timed_out {
            return None;
        }
        let settled_at = self.now_ms();
        let findings = {
            let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
            settle::evaluate(sut, post.as_ref(), &resolver)
        };
        for finding in findings {
            self.instance.record(finding);
        }
        Some(settled_at)
    }
}

/// What the generic close still holds open, in its own words: the settle's own
/// vocabulary for a scripted dialog that is not terminal yet (§10).
fn close_reasons(owed: &BTreeMap<String, Owed>) -> Vec<String> {
    owed.iter().map(|(leg, owed)| format!("the generic close: leg {leg} {owed}")).collect()
}
