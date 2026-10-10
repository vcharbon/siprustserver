//! A send step's message, composed by its leg's stack.

use std::collections::BTreeSet;

use pivot_schema::bundle::Failure;
use pivot_schema::deviation::CseqValue;
use sip_message::{Method, SipMessage, SipResponse};

use super::Runner;
use crate::deviation::StepEffects;
use crate::mscml::MscmlRequests;
use crate::plan::CompiledStep;
use crate::preserve;
use crate::render::{self};
use crate::resolve::Resolver;
use crate::stack::reason_for;

impl<'a, 'p> Runner<'a, 'p> {
    /// Keep one INVITE final per transaction on its leg, in arrival order. A
    /// second final of the same transaction REPLACES the first (a 2xx after a
    /// provisional-then-final race answers the same INVITE); a fork's own 2xx,
    /// which carries another To-tag, is its own entry.
    pub(super) fn note_final(&mut self, leg: &str, response: &SipResponse) {
        let key = |r: &SipResponse| {
            (r.cseq().seq(), r.to().tag().map(str::to_string).unwrap_or_default())
        };
        let pool = self.finals.entry(leg.to_string()).or_default();
        match pool.iter_mut().find(|held| key(held) == key(response)) {
            Some(held) => *held = response.clone(),
            None => pool.push(response.clone()),
        }
    }

    /// The final an auto ACK step acknowledges: the newest one still answering an
    /// un-ACKed INVITE this leg sent, else the newest answering any of them.
    ///
    /// LEG STATE names the transaction, never the step's captured `cseq` (§6.3)
    /// — the same resolution `Stack::ack_for` composes against, fallback
    /// included. RFC 3261 §14.1 leaves one INVITE outstanding per dialog, so a
    /// compliant peer offers one candidate. A captured peer that pipelined a
    /// second re-INVITE before ACKing the first's 2xx offers two, and each ACK
    /// DISCHARGES its own, so the deferred 2xx is still standing for the step
    /// that owes it rather than losing its ACK to the newer transaction. A
    /// captured peer that ACKed one 2xx TWICE (§13.2.2.4 obliges one ACK per 2xx
    /// received) leaves none outstanding, and the discharge must not deny the
    /// second step the final it is modelling an ACK of.
    pub(super) fn final_for(&self, step: &CompiledStep) -> Option<SipResponse> {
        let stack = self.stacks.get(&step.leg)?;
        let outstanding: BTreeSet<u32> = stack.outstanding_invites().collect();
        let sent: BTreeSet<u32> = stack.sent_invite_cseqs().collect();
        let pool = self.finals.get(&step.leg)?;
        let newest =
            |seqs: &BTreeSet<u32>| pool.iter().rev().find(|r| seqs.contains(&r.cseq().seq()));
        newest(&outstanding).or_else(|| newest(&sent)).cloned()
    }

    /// The CSeqs of every INVITE `leg` has sent, in send order: the transactions
    /// [`Self::final_for`] looks through, so a refusal names what it looked at.
    pub(super) fn sent_invites(&self, leg: &str) -> Vec<u32> {
        self.stacks.get(leg).map(|s| s.sent_invite_cseqs().collect()).unwrap_or_default()
    }

    /// Compose the message a send step emits. A response answers the request
    /// `answering` names — CSeq number and dialog To-tag — where given, else
    /// the leg's latest of its CSeq method.
    pub(super) fn compose(
        &mut self,
        step: &CompiledStep,
        effects: &StepEffects,
        answering: Option<(u32, Option<&str>)>,
    ) -> Result<SipMessage, Box<Failure>> {
        let site = format!("step {:?}", step.id);
        // The lane's per-call directives (§4.3) reach exactly one step: the
        // INVITE that opens this call's caller leg. Every other send carries the
        // run-wide headers alone.
        let call_headers = self
            .instance
            .plan()
            .call_dialled_by(&step.id)
            .and_then(|call| self.instance.config().headers_for_call(call))
            .cloned()
            .unwrap_or_default();
        let received = MscmlRequests::default();
        let (rendered, cseq_override) = {
            let resolver = Resolver::new(self.instance.state(), &self.instance.config().identities);
            let mscml = self.instance.state().leg(&step.leg).map_or(&received, |leg| &leg.mscml);
            let cx = render::Context {
                resolver: &resolver,
                config: self.instance.config(),
                effects,
                composer: self.lane.composer,
                base_dir: &self.lane.base_dir,
                media: &self.lane.media,
                leg: &step.leg,
                mscml,
                call_headers: &call_headers,
            };
            let rendered =
                render::render(&step.msg, &site, &cx).map_err(|e| Failure::SendFailed {
                    step: step.id.clone(),
                    leg: step.leg.clone(),
                    detail: e.to_string(),
                })?;
            // A relative override reads a CSeq the run already produced, so it
            // resolves HERE, against run state, and never against document text.
            let cseq_override = match &effects.cseq_override {
                None => None,
                Some((deviation, value)) => Some((
                    deviation.clone(),
                    match value {
                        CseqValue::Absolute(n) => *n,
                        CseqValue::Relative(computed) => {
                            resolver.computed(computed).map_err(|e| Failure::SendFailed {
                                step: step.id.clone(),
                                leg: step.leg.clone(),
                                detail: format!("cseq-override {deviation:?}: {e}"),
                            })?
                        }
                    },
                )),
            };
            (rendered, cseq_override)
        };
        // Composed once per emission: a retransmission re-sends these bytes and
        // answers nothing again.
        self.instance.leg_mut(&step.leg).mscml.answer(&rendered.mscml_answered);
        let body = rendered.template.body().to_vec();
        // A preserved emission (§11) keeps a stated Content-Type IN the stored
        // block: lifting it out would frame the body under a header the stack
        // places, which is exactly the reordering the deviation forbids.
        let preserved = !effects.preserve_stored_block.is_empty();
        let content_type = rendered
            .template
            .headers()
            .iter()
            .find(|h| sip_message::HeaderName::ContentType.matches(&h.name))
            .filter(|_| !preserved)
            .map(|h| h.value.clone());
        let headers: Vec<sip_message::TemplateHeader> = rendered
            .template
            .headers()
            .iter()
            .filter(|h| preserved || !sip_message::HeaderName::ContentType.matches(&h.name))
            .cloned()
            .collect();
        let fail = |detail: String| Failure::SendFailed {
            step: step.id.clone(),
            leg: step.leg.clone(),
            detail,
        };
        let acked_final = self.final_for(step);
        let sent_invites = self.sent_invites(&step.leg);
        let early_tag = self.fork_tag_of(step).map(str::to_string);
        let stack =
            self.stacks.get_mut(&step.leg).ok_or_else(|| fail("the leg has no stack".into()))?;
        if let Some(n) = step.msg.target_refresh {
            stack.refresh_target(n);
        }

        // Where the CSeq is NOT the stack's to choose, an override cannot be
        // honoured, and a message that quietly kept the compliant number would
        // not reproduce the defect (§11).
        if let Some((deviation, _)) = &cseq_override {
            let owned = crate::deviation::cseq_override_refusal(
                step.msg.status,
                step.msg.method.as_deref(),
            );
            if let Some(reason) = owned {
                return Err(Box::new(Failure::CseqOverrideRefused {
                    step: step.id.clone(),
                    leg: step.leg.clone(),
                    deviation: deviation.clone(),
                    reason: reason.to_string(),
                }));
            }
        }
        let cseq_override = cseq_override.map(|(_, value)| value);

        if let Some(status) = step.msg.status {
            let answer = crate::stack::Answer {
                status,
                reason: step.msg.reason.as_deref().unwrap_or_else(|| reason_for(status)),
                cseq_method: step.msg.cseq_method.as_deref(),
                early_tag: early_tag.as_deref(),
            };
            let response = match answering {
                Some((cseq, to_tag)) => {
                    stack.respond_to(cseq, to_tag, &answer, &headers, body, content_type)
                }
                None => stack.respond(&answer, &headers, body, content_type),
            }
            .map_err(|e| fail(e.to_string()))?;
            let message = SipMessage::Response(response);
            preserved_block(step, effects, &rendered, &message)?;
            return Ok(message);
        }
        let method = Method::from_wire(step.msg.method.as_deref().unwrap_or_default());
        let request = match method {
            // The stack owns the ACK's COMPOSITION — its CSeq is the INVITE's,
            // never the step's stored one, and only a `cseq-override` (§11)
            // states another — and the step owns its content: the frozen
            // headers, and on a 2xx the body a delayed offer's answer rides
            // (RFC 3261 §13.2.1).
            Method::Ack => {
                let response = acked_final.ok_or_else(|| {
                    fail(if sent_invites.is_empty() {
                        "no final response to ACK: this leg sent no INVITE".into()
                    } else {
                        let cseqs: Vec<String> = sent_invites.iter().map(u32::to_string).collect();
                        format!(
                            "no final response to ACK: nothing has answered any INVITE this \
                             leg sent, CSeq {}",
                            cseqs.join(", ")
                        )
                    })
                })?;
                stack
                    .ack_for(&response, &headers, body, content_type, cseq_override)
                    .map_err(|e| fail(e.to_string()))?
            }
            Method::Cancel => stack.cancel(&headers).map_err(|e| fail(e.to_string()))?,
            // The stack owns the RAck (RFC 3262 §7.2): it acknowledges a
            // provisional the leg RECEIVED, and a PRACK rides an early dialog
            // that is not confirmed, so it does not take the in-dialog path.
            // The fork the step names picks WHICH early dialog, as on UPDATE.
            Method::Prack => stack
                .prack(early_tag.as_deref(), &headers, body, content_type, cseq_override)
                .map_err(|e| fail(e.to_string()))?,
            // RFC 3311 §5.1: an UPDATE runs inside a dialog that may still be
            // early, so the stack decides between the confirmed dialog and the
            // fork the step names.
            Method::Update => stack
                .update(early_tag.as_deref(), &headers, body, content_type, cseq_override)
                .map_err(|e| fail(e.to_string()))?,
            _ if !stack.has_dialog() && stack.sent_invite().is_none() && !stack.rings_early() => {
                // A dialog-opening request is addressed at a party. Emitting one
                // with an empty Request-URI, From or To puts a malformed message
                // on the wire and calls it a replay.
                let missing: Vec<&str> = [
                    ("ruri", rendered.ruri.as_deref()),
                    ("from", rendered.from.as_deref()),
                    ("to", rendered.to.as_deref()),
                ]
                .iter()
                .filter(|(_, value)| value.is_none_or(str::is_empty))
                .map(|(what, _)| *what)
                .collect();
                if !missing.is_empty() {
                    return Err(Box::new(fail(format!(
                        "the dialog-opening {method} states no {}; the document's tier-2 refs \
                         resolve to nothing the lane can address",
                        missing.join(", ")
                    ))));
                }
                let addresses = crate::stack::Addresses {
                    ruri: rendered.ruri.as_deref().unwrap_or_default(),
                    from: rendered.from.as_deref().unwrap_or_default(),
                    to: rendered.to.as_deref().unwrap_or_default(),
                };
                stack
                    .out_of_dialog(method, &addresses, &headers, body, content_type, cseq_override)
                    .map_err(|e| fail(e.to_string()))?
            }
            // A request on a leg still ringing rides the early dialog the step
            // names, or the leg's only one (RFC 3261 §12.1.1): a callee's BYE
            // on the dialog its provisional opened is one (§15).
            _ => stack
                .in_dialog_on(
                    early_tag.as_deref(),
                    method,
                    &headers,
                    body,
                    content_type,
                    cseq_override,
                )
                .map_err(|e| fail(e.to_string()))?,
        };
        let message = SipMessage::Request(request);
        preserved_block(step, effects, &rendered, &message)?;
        Ok(message)
    }
}

/// Hold a preserved emission to the block the document stores (§11): a
/// `verbatim-emission` or `raw-order` step is VERIFIED against the message that
/// was composed, so the property is a guarantee and not an intention.
fn preserved_block(
    step: &CompiledStep,
    effects: &StepEffects,
    rendered: &render::Rendered,
    message: &SipMessage,
) -> Result<(), Box<Failure>> {
    let Some(deviation) = effects.preserve_stored_block.first() else { return Ok(()) };
    preserve::verify(rendered.template.headers(), message.headers()).map_err(|detail| {
        Box::new(Failure::EmissionNotPreserved {
            step: step.id.clone(),
            leg: step.leg.clone(),
            deviation: deviation.clone(),
            detail,
        })
    })
}
