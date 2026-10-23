//! What a leg owes outside its script: owed finals and ACKs, the §12.2.2
//! refusal, and the obligations no step scripts.

use pivot_schema::bundle::{Dir, Failure};
use scenario_harness::dialogs::DialogVerdict;
use sip_message::{Method, SipMessage};

use super::Runner;
use crate::close::{self, Owed, FOREIGN_DIALOG, FOREIGN_DIALOG_481};
use crate::gate::{self, Inbound};
use crate::plan::{CompiledStep, Discriminator};
use crate::scripted_final;
use crate::stack::{reason_for, via_target};

/// The site an emission the FLOW never scripted is attributed to: it belongs to
/// no step, because no step is what it answers.
const UNSCRIPTED: &str = "(unscripted)";

/// The recording note for the RFC 3261 §15.1.2 final the document never held.
pub(super) const OWED_BYE_FINAL: &str =
    "absorbed: the §15.1.2 final owed to the BYE this leg sent, which no expect scripts";

/// The recording note for the RFC 3261 §17.2.1 ACK to a reject no step scripts.
pub(super) const REJECT_ACK: &str =
    "absorbed: the §17.2.1 ACK to the non-2xx final this leg sent, which no expect scripts";

/// The recording note for the RFC 3261 §17.1.1.3 ACK the settle waited for.
pub(super) const OWED_FINAL_ACK: &str =
    "absorbed: the §17.1.1.3 ACK owed to the non-2xx final this leg sent, which the settle awaited";

/// Whether the step is the document's own answer to a BYE — a final gated on a
/// BYE transaction, whatever status it names.
fn scripts_a_bye_final(step: &CompiledStep) -> bool {
    matches!(
        &step.discriminator,
        Discriminator::Response { status, cseq_method: Some(method) }
            if *status >= 200 && method.eq_ignore_ascii_case("BYE")
    )
}

impl<'a, 'p> Runner<'a, 'p> {
    /// Whether `inbound` is the final response a BYE this leg SENT is owed.
    /// RFC 3261 §15.1.2 obliges the UAS to answer a BYE, so the answer is a
    /// protocol fact and not a choreography the document must hold — a capture
    /// whose vantage closed between the BYE and its answer scripts no expect for
    /// it, and the leg that tore the dialog down has nothing left to await.
    /// A document that DOES script the final still owns it, WHATEVER status it
    /// scripts: absorbing a final the document disagrees with would eat the
    /// divergence the confrontation exists to name.
    pub(super) fn owed_bye_final(&self, leg: &str, inbound: &Inbound) -> bool {
        if !inbound.status.is_some_and(|status| status >= 200)
            || !inbound.cseq_method.eq_ignore_ascii_case("BYE")
            || !self.byes_sent.contains(&(leg.to_string(), inbound.cseq))
        {
            return false;
        }
        !self.instance.plan().steps().iter().any(|step| {
            step.is_expect()
                && step.leg == leg
                && (scripts_a_bye_final(step) || gate::discriminates(step, inbound).matches())
        })
    }

    /// Whether `inbound` is an ACK to a non-2xx INVITE final this leg SENT
    /// (RFC 3261 §17.1.1.3): its server transaction's own, which the settle
    /// floor holds the run open for, so it is the settle's arrival and never
    /// the late datagram a completed flow reports — a repeat included.
    pub(super) fn owed_final_ack(&self, leg: &str, inbound: &Inbound) -> bool {
        if !inbound.method.as_deref().is_some_and(|method| method.eq_ignore_ascii_case("ACK")) {
            return false;
        }
        let ladder = self.instance.recording().legs().remove(leg).unwrap_or_default();
        scripted_final::rejected(&ladder, inbound.cseq)
    }

    /// Whether `inbound` is an ACK to a non-2xx INVITE final this leg sent that
    /// no pending `expect` scripts ([`scripted_final::scripts_reject_ack`]):
    /// hop by hop, its server transaction consumes it (RFC 3261 §17.2.1), so it
    /// is neither unexpected nor a match for another step.
    pub(super) fn unscripted_reject_ack(&self, leg: &str, inbound: &Inbound) -> bool {
        if !self.owed_final_ack(leg, inbound) {
            return false;
        }
        let ladder = self.instance.recording().legs().remove(leg).unwrap_or_default();
        let steps = self.instance.plan().steps();
        let cursor = self.instance.cursor();
        !scripted_final::scripts_reject_ack(&steps, leg, inbound.cseq, &ladder, |id| {
            cursor.node_complete(id)
        })
    }

    /// Whether `message` is a non-2xx INVITE final the step `took` took or was
    /// retired by, and `leg`'s flow scripts no ACK for
    /// ([`scripted_final::scripts_final_ack`]).
    /// RFC 3261 §17.1.1.3 makes that ACK the client transaction's own, so a
    /// document stating none stopped where its capture did and the leg sends
    /// it all the same.
    pub(super) fn unscripted_final_ack(&self, leg: &str, took: &str, message: &SipMessage) -> bool {
        let SipMessage::Response(response) = message else { return false };
        if response.status() < 300 || *response.cseq().method() != Method::Invite {
            return false;
        }
        let steps = self.instance.plan().steps();
        let cursor = self.instance.cursor();
        !scripted_final::scripts_final_ack(&steps, leg, took, |id| cursor.node_complete(id))
    }

    /// The leg of `message` and the verdict the dialog table of the UA that
    /// received it (`actor`'s) passed on it, when it is a request with one
    /// (RFC 3261 §12.2.2).
    pub(super) fn dialog_verdict(
        &self,
        actor: &str,
        message: &SipMessage,
    ) -> Option<(String, DialogVerdict)> {
        let SipMessage::Request(request) = message else { return None };
        let leg = self.leg_by_call_id(request.call_id().as_str())?;
        let verdict = self.lane.agents.get(actor)?.dialog_verdict(request)?;
        Some((leg, verdict))
    }

    /// Fail the run on a dialog verdict the leg takes on the wire (a CSeq
    /// skip, RFC 3261 §12.2.1.1): the request is still dispatched.
    pub(super) fn report_dialog_verdict(
        &mut self,
        leg: &str,
        message: &SipMessage,
        verdict: &DialogVerdict,
    ) {
        self.instance.fail(Failure::UnexpectedDatagram {
            leg: leg.to_string(),
            arrived: Inbound::of(message).arrived(),
            detail: Some(verdict.to_string()),
        });
    }

    /// Answer `message` 481 (RFC 3261 §12.2.2): it names no dialog its leg
    /// holds (none, or one that ended), so it is recorded, refused, taught to
    /// nothing, and failed as the unexpected datagram it is — mid-flow and
    /// during settle alike. `false` when the 481 could not be sent.
    pub(super) async fn refuse_foreign_dialog(
        &mut self,
        actor: &str,
        leg: &str,
        message: &SipMessage,
        bytes: Vec<u8>,
        repeat: bool,
    ) -> bool {
        let SipMessage::Request(request) = message else { return true };
        self.record_arrival(leg, bytes, None, Some(FOREIGN_DIALOG), repeat);
        self.instance.fail(Failure::UnexpectedDatagram {
            leg: leg.to_string(),
            arrived: Inbound::of(message).arrived(),
            detail: Some(FOREIGN_DIALOG.to_string()),
        });
        let Some(agent) = self.lane.agents.get(actor).cloned() else { return true };
        let response = sip_message::generators::generate_response(
            request,
            481,
            reason_for(481),
            &sip_message::generators::GenerateResponseOpts::default(),
        );
        let response = match response {
            Ok(response) => response,
            Err(e) => {
                self.instance.fail(Failure::SendFailed {
                    step: "(foreign dialog)".into(),
                    leg: leg.to_string(),
                    detail: e.to_string(),
                });
                return false;
            }
        };
        let wire = sip_message::serialize(&SipMessage::Response(response));
        let dst = via_target(request).unwrap_or(self.lane.route_target);
        match agent.try_send_datagram(&wire, dst).await {
            Ok(()) => {
                self.instance.recording().push(
                    leg,
                    Dir::Out,
                    self.now_us(),
                    wire,
                    None,
                    Some(FOREIGN_DIALOG_481),
                );
                true
            }
            Err(e) => {
                self.instance.fail(Failure::SendFailed {
                    step: "(foreign dialog)".into(),
                    leg: leg.to_string(),
                    detail: e.to_string(),
                });
                false
            }
        }
    }

    /// Discharge the transaction-layer obligation a REFUSED datagram, or one
    /// arriving after the flow, leaves on its leg (RFC 3261 §9.2, §15.1.2,
    /// §17.1.1.3; RFC 3262 §3).
    ///
    /// The refusal STANDS and the flow is untouched: like a `background` answer
    /// this moves no cursor and satisfies no `expect`. What is owed is
    /// [`close::unscripted`]'s to say, so a method a document declared is never
    /// answered here, and the emission is the leg's own stack — the same
    /// compliant SIP the generic close puts on the wire.
    pub(super) async fn answer_unscripted(&mut self, leg: &str, message: &SipMessage) {
        // The CANCEL pair and the BYE pair are two acts each, and the second is
        // read off the recording once the first is on it — the turn the generic
        // close takes, taken here.
        for _ in 0..2 {
            let ladder = self.instance.recording().legs().remove(leg).unwrap_or_default();
            let Some(owed) = close::unscripted(&ladder, message) else { return };
            // A CANCELled INVITE the flow still owes its own reject draws that
            // step at once, its dwell not waited on (RFC 3261 §9.2: any non-2xx
            // may answer it); the flow walks on from it as from any send.
            if let Owed::Answer { cseq_method, status: 487, cseq, to_tag, .. } = &owed {
                if cseq_method == "INVITE" {
                    if let Some(step) = self.pending_reject(leg, *cseq) {
                        if !self.emit(&step, Some((*cseq, to_tag.clone()))).await {
                            self.end_script(Some(leg), Some(&step.id));
                        }
                        return;
                    }
                }
            }
            let Some(agent) = self.agent_of_leg(leg) else { return };
            let (wire, dst) = match self.compose_close(leg, &owed, UNSCRIPTED) {
                Ok(composed) => composed,
                Err(failure) => {
                    self.instance.fail(*failure);
                    return;
                }
            };
            if let Err(e) = agent.try_send_datagram(&wire, dst).await {
                self.instance.fail(Failure::SendFailed {
                    step: UNSCRIPTED.into(),
                    leg: leg.to_string(),
                    detail: format!("the transaction obligation could not be sent: {e}"),
                });
                return;
            }
            // An emission the run authored ends a ladder as a scripted one does.
            self.repeats.answered(leg, &wire, self.now_us());
            self.instance.recording().push(
                leg,
                Dir::Out,
                self.now_us(),
                wire,
                None,
                Some(&format!("the transaction the flow never scripted: this leg {owed}")),
            );
        }
    }

    /// The reject `leg`'s flow still owes its INVITE CSeq `cseq`
    /// ([`scripted_final::pending_reject`]).
    pub(super) fn pending_reject(&self, leg: &str, cseq: u32) -> Option<CompiledStep> {
        let ladder = self.instance.recording().legs().remove(leg).unwrap_or_default();
        let steps = self.instance.plan().steps();
        let cursor = self.instance.cursor();
        scripted_final::pending_reject(&steps, leg, cseq, &ladder, |id| cursor.node_complete(id))
            .cloned()
    }
}
