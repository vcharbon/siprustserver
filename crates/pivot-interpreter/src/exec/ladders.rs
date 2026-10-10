//! The retransmission ladders a run paces, draws and counts (§6.3, §6.9).

use pivot_schema::bundle::{Dir, Failure};
use scenario_harness::absorption::{ack_key_of_ack, ack_key_of_final, AckKey};
use scenario_harness::Agent;
use sip_message::SipMessage;
use sip_retransmit::Schedule;
use tokio::time::Instant;

use super::{PendingRepeat, Runner};
use crate::plan::CompiledStep;
use crate::retransmit::DrawnAck;
use crate::scope::Finding;

impl<'a, 'p> Runner<'a, 'p> {
    /// Emit one more copy of an ACK that a repeat of its final drew (§6.3): the
    /// SAME datagram on the SAME client transaction, since RFC 3261 §13.2.2.4
    /// owes one ACK per 2xx received and every one of them answers the INVITE
    /// the first 2xx armed.
    pub(super) async fn draw_ack(&mut self, ack: &DrawnAck) -> bool {
        let Some(agent) = self.agent_of_leg(&ack.leg) else {
            self.instance.fail(Failure::SendFailed {
                step: ack.step.clone(),
                leg: ack.leg.clone(),
                detail: "the leg names no bound agent to draw its ACK from".into(),
            });
            return false;
        };
        if let Err(e) = agent.try_send_datagram(&ack.wire, ack.dst).await {
            self.instance.fail(Failure::SendFailed {
                step: ack.step.clone(),
                leg: ack.leg.clone(),
                detail: format!("drawing an ACK for a repeated final: {e}"),
            });
            return false;
        }
        self.instance.recording().push_repeat(
            &ack.leg,
            Dir::Out,
            self.now_us(),
            ack.wire.clone(),
            Some(&ack.step),
            Some("drawn by a repeat of the final it acknowledges"),
        );
        self.repeats.answered(&ack.leg, &ack.wire, self.now_us());
        self.repeats.note(&ack.leg, &ack.wire, self.now_us());
        true
    }

    /// A repeat of a 2xx draws the ACK an auto step already sent for it, where
    /// the document declared that ladder. Every other repeat draws nothing.
    pub(super) async fn draw_for_repeat(&mut self, message: &SipMessage) -> bool {
        let SipMessage::Response(response) = message else { return true };
        let Some(key) = ack_key_of_final(response) else { return true };
        let Some(ack) = self.drawn.repeat_sighted(key) else { return true };
        self.draw_ack(&ack).await
    }

    /// Book a send step's declared retransmission ladder (§6.9): `count` more
    /// copies of the same bytes, each at the instant `schedule` paces it, counting
    /// from the copy just sent. The loop emits them; nothing here waits.
    ///
    /// The peer stays RFC-compliant while it does it — the copies are identical,
    /// and the intervals are the RFC's own — and each one is recorded pointing
    /// at the datagram it repeats. The flow DOES advance meanwhile: a ladder
    /// paces one leg's transaction and gates nothing else, so sleeping the rungs
    /// out here would park every other leg's dwell and every arrival's turn
    /// behind a timer that was never a barrier.
    pub(super) fn schedule_ladder(
        &mut self,
        step: &CompiledStep,
        agent: &Agent,
        wire: &[u8],
        dst: std::net::SocketAddr,
        schedule: Schedule,
        count: u32,
    ) {
        let mut at = Instant::now();
        for n in 1..=count {
            // `schedule_of` refuses a class with no rung, so every rung a
            // declared count names has its wait.
            let Some(wait) = schedule.interval(n) else { break };
            at += wait;
            self.pending_repeats.push(PendingRepeat {
                step: step.id.clone(),
                leg: step.leg.clone(),
                agent: agent.clone(),
                wire: wire.to_vec(),
                dst,
                at,
                n,
                count,
            });
        }
    }

    /// The soonest rung still owed, for the loop's own wake calculus.
    pub(super) fn next_repeat(&self) -> Option<Instant> {
        self.pending_repeats.iter().map(|r| r.at).min()
    }

    /// Emit every rung now due, oldest first. `false` once a send fails.
    pub(super) async fn drain_repeats(&mut self) -> bool {
        loop {
            let now = Instant::now();
            let Some(i) = self
                .pending_repeats
                .iter()
                .enumerate()
                .filter(|(_, r)| r.at <= now)
                .min_by_key(|(_, r)| (r.at, r.n))
                .map(|(i, _)| i)
            else {
                return true;
            };
            let due = self.pending_repeats.remove(i);
            if let Err(e) = due.agent.try_send_datagram(&due.wire, due.dst).await {
                self.instance.fail(Failure::SendFailed {
                    step: due.step.clone(),
                    leg: due.leg.clone(),
                    detail: format!("retransmission {} of {}: {e}", due.n, due.count),
                });
                return false;
            }
            self.instance.recording().push_repeat(
                &due.leg,
                Dir::Out,
                self.now_us(),
                due.wire.clone(),
                Some(&due.step),
                Some(&format!("retransmission {} of {}", due.n, due.count)),
            );
            self.repeats.note(&due.leg, &due.wire, self.now_us());
        }
    }

    /// The ladders the run counted (§6.9), into the verdict — and a ladder that
    /// is not the one its emitter owed is a failure, on every lane: a
    /// retransmission count is a protocol fact, never a lane's vocabulary.
    pub(super) fn reconcile_retransmits(&mut self) {
        // The run's own end bounds a ladder nothing closed (§6.9): it ran until
        // the run stopped, and that is the window its rungs are owed inside.
        let run_end_us = self.now_us();
        let notes = self.repeats.notes(run_end_us);
        for failure in self.repeats.mismatches(run_end_us) {
            self.instance.record(Finding::gating(failure));
        }
        self.instance.note_retransmits(notes);
    }
}

/// The dialog an auto ACK step's DRAWN ladder is keyed on (§6.3), or `None`
/// wherever a count is not drawn.
///
/// Three conditions, and each is the ruling's: only an ACK's count is drawn
/// (every other message has an RFC ladder to ride), only an `auto` step's is
/// (the stack owns the ACK), and only one the document MARKS with `cseq` (§6.3
/// — a count is measured against the repeats of one transaction's final).
pub(super) fn drawn_key(step: &CompiledStep, message: &SipMessage) -> Option<AckKey> {
    if !step.auto || step.msg.cseq.is_none() || step.retransmits.is_none_or(|n| n == 0) {
        return None;
    }
    match message {
        SipMessage::Request(r) => ack_key_of_ack(r),
        SipMessage::Response(_) => None,
    }
}
