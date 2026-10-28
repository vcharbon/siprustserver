//! One datagram arriving mid-flow: absorbed, counted, refused, answered by a
//! policy, held, or delivered.

use std::time::Duration;

use pivot_schema::bundle::Dir;
use scenario_harness::absorption::Owner;
use sip_message::{Method, SipMessage};
use tokio::time::Instant;

use super::arbitration::{HELD_NOTE, HOLD_OUT_MS};
use super::{Held, Runner};
use crate::gate::Inbound;

impl<'a, 'p> Runner<'a, 'p> {
    /// Record, absorb-or-surface, attribute and dispatch one arriving datagram.
    pub(super) async fn dispatch(&mut self, actor: &str, message: SipMessage) -> bool {
        let inbound = Inbound::of(&message);
        let bytes: Vec<u8> = match &message {
            SipMessage::Request(r) => r.image().to_vec(),
            SipMessage::Response(r) => r.image().to_vec(),
        };

        // §17.2 FIRST, and before any claim: a retransmitted INVITE must not
        // consume a second claim on its way to being absorbed. The datagram is
        // still recorded — that is why the rule runs here and not below the
        // agent's own API.
        let sighting = self.absorption.sight(&bytes, &message, tokio::time::Instant::now());
        let repeat = sighting.repeat;
        if sighting.owner == Owner::TxnDuplicate {
            let leg =
                self.leg_by_call_id(&inbound.call_id).unwrap_or_else(|| "unattributed".to_string());
            self.repeats.note(&leg, &bytes, self.now_us());
            self.record_arrival(
                &leg,
                bytes,
                None,
                Some("absorbed: byte-identical retransmission of a datagram already surfaced"),
                repeat,
            );
            return true;
        }

        // A repeat the transaction layer does NOT absorb — a 2xx to an INVITE,
        // an ACK, a replayed provisional — still satisfies no second expect: the
        // step it repeats has already matched and the flow has moved on. Where
        // the document DECLARES the ladder (§6.9) it is counted and recorded;
        // where it does not, it falls through to the gate and fails there, which
        // is what an unannounced repeat is.
        if let Some(leg) = self.leg_by_call_id(&inbound.call_id) {
            if let Some(step) = self.repeats.note(&leg, &bytes, self.now_us()) {
                // A DECLARED ladder's repeat, always recorded as one: §6.9 counts
                // the repeats of a step's own message, which is not the same unit
                // as §17.2's duplicate — a repeated provisional is deliberately
                // never a duplicate, and it is still this step's retransmission.
                let note = format!("retransmission of the datagram step {step:?} matched");
                self.instance.recording().push_repeat(
                    &leg,
                    Dir::In,
                    self.now_us(),
                    bytes,
                    None,
                    Some(&note),
                );
                // A repeated 2xx draws its own ACK (§6.3): the seam has already
                // ruled it visible to the transaction user, so the UAC core owes
                // one more copy of the ACK it sent.
                if !self.draw_for_repeat(&message).await {
                    self.end_script(Some(&leg), None);
                    return false;
                }
                return true;
            }
        }

        // RFC 3261 §12.2.2 before anything reads it: a request naming no dialog
        // its leg holds, or one that ended, is answered 481, and neither a
        // policy nor the flow takes it. A CSeq skip is reported and taken.
        if let Some((leg, verdict)) = self.dialog_verdict(actor, &message) {
            if verdict.kind.refusal().is_some() {
                return self.refuse_foreign_dialog(actor, &leg, &message, bytes, repeat).await;
            }
            self.report_dialog_verdict(&leg, &message, &verdict);
        }

        // Before any policy or step consumes it: a response the leg sends later
        // echoes the id of the MSCML request it answers (RFC 5022 §10.2). An
        // initial INVITE is remembered by `deliver`, once its claim names the leg.
        if !opens_dialog(&inbound) {
            if let Some(leg) = self.leg_by_call_id(&inbound.call_id) {
                self.instance.leg_mut(&leg).mscml.remember(&inbound);
            }
        }

        // Background next (§5.1): a policy's traffic never touches the cursor.
        // The policy is sought over every actor on the RECEIVING endpoint, not
        // only the one whose pump surfaced it: a loopback endpoint hosts a UAC
        // and a UAS on one socket, and only one of them is pumped.
        let known_leg = self.leg_by_call_id(&inbound.call_id);
        let awaited = known_leg.as_deref().is_some_and(|leg| self.flow_awaits(leg, &inbound));
        if let Some(method) = &inbound.method {
            if let Some((index, policy)) = (!awaited)
                .then(|| self.background_policy(actor, method, known_leg.as_deref()))
                .flatten()
            {
                let leg = known_leg.clone().unwrap_or_else(|| policy.actor.clone());
                self.record_arrival(&leg, bytes, None, Some("background policy"), repeat);
                self.instance.note_background_answered(index);
                if !self.answer_background(actor, &leg, &message, policy.status).await {
                    self.end_script(Some(&leg), None);
                    return false;
                }
                return true;
            }
        }

        // §5.1 arbitration, where the window is not enough to tell the two
        // apart: a step and a policy can BOTH want this method at this instant,
        // and arrival order is not evidence of which one it is. A step that
        // froze headers and got a datagram carrying none of them holds out for
        // one that carries them, and gives up the moment nothing better lands.
        if let Some(step) = self.holds_out_for_better(known_leg.as_deref(), actor, &inbound) {
            let leg = known_leg.clone().unwrap_or_default();
            self.record_arrival(&leg, bytes.clone(), None, Some(HELD_NOTE), repeat);
            let seq = self.instance.recording().last_seq(&leg);
            self.held = Some(Held {
                actor: actor.to_string(),
                leg,
                step,
                inbound,
                message,
                bytes,
                repeat,
                seq,
                release_at: Instant::now() + Duration::from_millis(HOLD_OUT_MS),
            });
            return true;
        }

        self.deliver(actor, inbound, message, bytes, repeat, None).await
    }
}

/// Whether `inbound` is an initial INVITE: no To-tag yet, so no dialog the
/// leg could be found by before a claim names it (RFC 3261 §12.1).
pub(super) fn opens_dialog(inbound: &Inbound) -> bool {
    inbound.method.as_deref().map(Method::from_wire) == Some(Method::Invite)
        && inbound.to_tag.is_none()
}
