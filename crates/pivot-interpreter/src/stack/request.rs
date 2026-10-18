//! The requests a leg composes: dialog-opening, in-dialog, UPDATE, PRACK, ACK and CANCEL.

use super::{frozen, media_type, Addresses, LegStack, StackError};
use sip_message::generators::{
    generate_ack_for_2xx, generate_ack_for_non_2xx, generate_cancel, generate_in_dialog_request,
    generate_out_of_dialog_request, GenerateAckFor2xxOpts, GenerateInDialogRequestOpts,
    GenerateOutOfDialogRequestOpts, InDialogMethod, InviteClientTransactionHandle,
    OutOfDialogMethod, StackDialog,
};
use sip_message::header::{self, NameAddr, Uri};
use sip_message::{Method, SipHeader, SipRequest, SipResponse, SipStr};
use sip_template::TemplateHeader;

impl LegStack {
    /// Compose the dialog-opening request of an originating leg. The R-URI,
    /// From and To are the lane's tier-2 values; everything else is tier 1.
    ///
    /// `cseq` is a `cseq-override` (§11): it BECOMES the leg's sequence number,
    /// because RFC 3261 §12.2.1.1 numbers every later request from the dialog's
    /// own counter and the transaction this request opens is identified by what
    /// went out.
    pub fn out_of_dialog(
        &mut self,
        method: Method,
        addresses: &Addresses<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        let (ruri, from, to) = (addresses.ruri, addresses.from, addresses.to);
        let Some(verb) = out_of_dialog_method(&method) else {
            let (leg, name) = (self.leg.clone(), method.to_string());
            // A request that only ever rides a dialog, asked for outside one,
            // names a leg with no dialog rather than a method with no recipe.
            return Err(match in_dialog_method(&method) {
                Some(_) => StackError::NoDialog { leg, method: name },
                None => StackError::UnsupportedMethod { leg, method: name },
            });
        };
        self.dialog.local_uri = from.to_string();
        self.dialog.remote_uri = to.to_string();
        self.dialog.local_cseq = cseq.unwrap_or(self.dialog.local_cseq + 1);
        let via = self.via();
        let opts = GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of(ruri)),
            call_id: self.dialog.call_id.clone(),
            from: Some(
                header::From::new(addr_of(from)).with_tag(SipStr::owned(&self.dialog.local_tag)),
            ),
            to: Some(header::To::new(addr_of(to))),
            cseq: self.dialog.local_cseq,
            via: Some(via),
            contact: Some(self.contact()),
            max_forwards: None,
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: frozen(headers),
        };
        let request = generate_out_of_dialog_request(verb, &opts)
            .map_err(|e| StackError::unbuilt(&self.leg, e))?;
        if method == Method::Invite {
            self.sent_invites.push(request.clone());
        }
        Ok(request)
    }

    /// Compose an in-dialog request. `cseq` is a `cseq-override` (§11) and
    /// becomes the dialog's sequence number, as it does on a dialog-opening
    /// request. With no dialog a 2xx opened, it rides the leg's only early
    /// dialog, still ringing or, for any method but INVITE, ended by a non-2xx
    /// final ([`in_dialog_on`](Self::in_dialog_on)).
    pub fn in_dialog(
        &mut self,
        method: Method,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        self.in_dialog_on(None, method, headers, body, content_type, cseq)
    }

    /// Compose an in-dialog request on the dialog the leg has: the one a 2xx
    /// opened, else the early dialog `early` names — the leg's only one where
    /// it names none (RFC 3261 §12.1.1: a UAS's provisional under a To-tag
    /// opens an early dialog on its side too, and a BYE may ride it, §15). A
    /// dialog with no remote target is refused: there is nothing to address.
    pub fn in_dialog_on(
        &mut self,
        early: Option<&str>,
        method: Method,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        let verb = in_dialog_method(&method).ok_or_else(|| StackError::UnsupportedMethod {
            leg: self.leg.clone(),
            method: method.to_string(),
        })?;
        let fork = if self.has_dialog { None } else { Some(self.early_tag_for(early, &method)?) };
        let dialog = match &fork {
            None => self.dialog.clone(),
            Some(tag) => self.early_dialog(tag).map(|e| e.dialog.clone()).ok_or_else(|| {
                StackError::EarlyDialogUnknown {
                    leg: self.leg.clone(),
                    method: method.to_string(),
                    early: tag.clone(),
                }
            })?,
        };
        self.addressable(&dialog, &method)?;
        let via = self.via();
        let opts = GenerateInDialogRequestOpts {
            via: Some(via),
            contact: Some(self.contact()),
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: frozen(headers),
            cseq,
            ..Default::default()
        };
        let result = generate_in_dialog_request(verb, &dialog, &opts)
            .map_err(|e| StackError::unbuilt(&self.leg, e))?;
        match fork {
            None => self.dialog = result.dialog,
            Some(tag) => {
                if let Some((_, early)) = self.early.iter_mut().find(|(known, _)| *known == tag) {
                    early.dialog = result.dialog;
                }
            }
        }
        if method == Method::Invite {
            self.sent_invites.push(result.request.clone());
        }
        Ok(result.request)
    }

    /// The UPDATE this leg sends (RFC 3311 §5.1).
    ///
    /// An UPDATE rides a CONFIRMED dialog where the leg has one. Before
    /// confirmation it rides an EARLY dialog, taking that fork's remote tag,
    /// remote target and route set: `early` names which, and a leg ringing one
    /// fork needs no name. Several forks and no name is a refusal — which
    /// dialog an UPDATE rides decides which endpoint it reaches, and §14 has the
    /// tool infer nothing.
    pub fn update(
        &mut self,
        early: Option<&str>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        self.in_dialog_on(early, Method::Update, headers, body, content_type, cseq)
    }

    /// The PRACK this leg owes a reliable provisional it received (RFC 3262
    /// §7.2): the oldest un-acknowledged one OF THE FORK the step names, or the
    /// oldest the leg holds where it names none. The `RAck` is composed from
    /// that provisional — never from a number the document repeats — unless the
    /// step froze one, which wins as every frozen header does.
    ///
    /// It rides the EARLY dialog the acknowledged provisional opened, exactly
    /// as [`LegStack::update`] rides the fork it names: composed from that
    /// fork's dialog and numbered in its own sequence (RFC 3261 §12.2.1.1), so
    /// the To-tag and the RAck name ONE dialog. A PRACK is in-dialog on a
    /// dialog that is not confirmed yet; once the leg's dialog confirms, it
    /// rides that. A dialog with no remote target refuses it.
    pub fn prack(
        &mut self,
        early: Option<&str>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        if let Some(named) = early {
            if !self.confirmed && self.early_dialog(named).is_none() {
                return Err(StackError::EarlyDialogUnknown {
                    leg: self.leg.clone(),
                    method: Method::Prack.to_string(),
                    early: named.to_string(),
                });
            }
        }
        let chosen = match early {
            Some(named) => {
                self.unacknowledged.iter().position(|p| p.to_tag.as_deref() == Some(named))
            }
            None => (!self.unacknowledged.is_empty()).then_some(0),
        };
        let mut extra = frozen(headers);
        if !extra.iter().any(|h| sip_message::HeaderName::RAck.matches(&h.name)) {
            let provisional = chosen
                .map(|at| &self.unacknowledged[at])
                .ok_or(StackError::NoReliableProvisional { leg: self.leg.clone() })?;
            extra.push(SipHeader::new("RAck", provisional.rack()));
        }
        let fork = (!self.confirmed)
            .then(|| match early {
                Some(named) => Some(named.to_string()),
                None => chosen.and_then(|at| self.unacknowledged[at].to_tag.clone()),
            })
            .flatten()
            .filter(|tag| self.early_dialog(tag).is_some());
        let dialog = fork
            .as_deref()
            .and_then(|tag| self.early_dialog(tag))
            .map(|e| e.dialog.clone())
            .unwrap_or_else(|| self.dialog.clone());
        self.addressable(&dialog, &Method::Prack)?;
        if let Some(at) = chosen {
            self.unacknowledged.remove(at);
        }
        let via = self.via();
        let opts = GenerateInDialogRequestOpts {
            via: Some(via),
            contact: Some(self.contact()),
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: extra,
            cseq,
            ..Default::default()
        };
        let result = generate_in_dialog_request(InDialogMethod::Prack, &dialog, &opts)
            .map_err(|e| StackError::unbuilt(&self.leg, e))?;
        match fork {
            Some(tag) => {
                if let Some((_, early)) = self.early.iter_mut().find(|(known, _)| *known == tag) {
                    early.dialog = result.dialog;
                }
            }
            None => self.dialog = result.dialog,
        }
        Ok(result.request)
    }

    /// The ACK the stack owes a final response — the automatic an `auto` step
    /// marks. A 2xx ACK is a new transaction on the dialog; a non-2xx ACK rides
    /// the INVITE's own branch (RFC 3261 §17.1.1.3).
    ///
    /// The RESPONSE names the transaction: an ACK carries the CSeq of the
    /// INVITE the final answered, never the number of whichever INVITE the leg
    /// happened to send last. The two differ only where a captured peer broke
    /// RFC 3261 §14.1 and left two INVITEs outstanding at once — and there,
    /// composing against the newest silently re-ACKs one transaction and leaves
    /// the other's 2xx unacknowledged for the life of the dialog.
    ///
    /// The step's stored content rides it: the frozen headers on both arms, and
    /// the BODY on the 2xx arm alone, which is where a delayed offer's answer
    /// travels (RFC 3261 §13.2.1). A non-2xx ACK is absorbed by the INVITE
    /// transaction and reaches no TU that could read one, so the document may
    /// not store one there (§6.3) and this arm carries none.
    ///
    /// `cseq` is a `cseq-override` (§11) and changes ONLY the number the wire
    /// carries: the INVITE this ACK acknowledges is still found by the
    /// response's own CSeq, and is still the transaction the emission rides.
    /// The 2xx arm alone honours it — a non-2xx ACK is composed by the INVITE
    /// client transaction (RFC 3261 §17.1.1.3), whose number is not the core's
    /// to state, so an override there is refused rather than dropped.
    ///
    /// The 2xx arm is addressed to the dialog's remote target (RFC 3261
    /// §13.2.2.4), and a dialog with none refuses it.
    pub fn ack_for(
        &mut self,
        response: &SipResponse,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        cseq: Option<u32>,
    ) -> Result<SipRequest, StackError> {
        let invite = self
            .sent_invites
            .iter()
            .rev()
            .find(|invite| invite.cseq().seq() == response.cseq().seq())
            .or_else(|| self.sent_invites.last())
            .cloned()
            .ok_or_else(|| StackError::NoInvite { leg: self.leg.clone(), method: "ACK".into() })?;
        if response.status() < 300 {
            // The ACK rides the dialog the 2xx created, under ITS To-tag (RFC
            // 3261 §13.2.2.4) — not whatever tag the leg learned since.
            let dialog = match response.to().tag() {
                Some(tag) => StackDialog { remote_tag: tag.to_string(), ..self.dialog.clone() },
                None => self.dialog.clone(),
            };
            self.addressable(&dialog, &Method::Ack)?;
            self.acked_invites.insert(invite.cseq().seq());
            let txn = InviteClientTransactionHandle { original_invite: invite };
            let via = self.via();
            let opts = GenerateAckFor2xxOpts {
                via: Some(via),
                body,
                content_type: content_type.as_deref().map(media_type),
                extra_headers: frozen(headers),
                cseq,
                ..Default::default()
            };
            generate_ack_for_2xx(Some(&txn), &dialog, &opts)
                .map_err(|e| StackError::unbuilt(&self.leg, e))
        } else {
            self.acked_invites.insert(invite.cseq().seq());
            if cseq.is_some() {
                Err(StackError::CseqOverrideOnAbsorbedAck { leg: self.leg.clone() })
            } else {
                generate_ack_for_non_2xx(&invite, response, &frozen(headers))
                    .map_err(|e| StackError::unbuilt(&self.leg, e))
            }
        }
    }

    /// Refuse a request on a dialog with no remote target: no response that
    /// opened it carried a Contact (RFC 3261 §12.1.2), so there is no
    /// Request-URI to address it to.
    fn addressable(&self, dialog: &StackDialog, method: &Method) -> Result<(), StackError> {
        if dialog.remote_target.is_empty() {
            return Err(StackError::NoRemoteTarget {
                leg: self.leg.clone(),
                method: method.to_string(),
            });
        }
        Ok(())
    }

    /// The CANCEL for this leg's pending INVITE.
    pub fn cancel(&mut self, headers: &[TemplateHeader]) -> Result<SipRequest, StackError> {
        let invite = self.sent_invite().cloned().ok_or_else(|| StackError::NoInvite {
            leg: self.leg.clone(),
            method: "CANCEL".into(),
        })?;
        let txn = InviteClientTransactionHandle { original_invite: invite };
        generate_cancel(&txn, &frozen(headers)).map_err(|e| StackError::unbuilt(&self.leg, e))
    }
}

/// The URI a lane names as text. A value the reader accepts IS the URI; one it
/// does not is carried whole, so the peer sees what the lane wrote.
fn uri_of(text: &str) -> Uri {
    Uri::parse_or_verbatim(&SipStr::owned(text))
}

fn addr_of(text: &str) -> NameAddr {
    NameAddr::parse(&SipStr::owned(text)).unwrap_or_else(|_| NameAddr::new(uri_of(text)))
}

fn out_of_dialog_method(method: &Method) -> Option<OutOfDialogMethod> {
    Some(match method {
        Method::Invite => OutOfDialogMethod::Invite,
        Method::Options => OutOfDialogMethod::Options,
        Method::Message => OutOfDialogMethod::Message,
        Method::Subscribe => OutOfDialogMethod::Subscribe,
        Method::Register => OutOfDialogMethod::Register,
        _ => return None,
    })
}

fn in_dialog_method(method: &Method) -> Option<InDialogMethod> {
    Some(match method {
        Method::Invite => InDialogMethod::Invite,
        Method::Update => InDialogMethod::Update,
        Method::Bye => InDialogMethod::Bye,
        Method::Info => InDialogMethod::Info,
        Method::Message => InDialogMethod::Message,
        Method::Prack => InDialogMethod::Prack,
        Method::Notify => InDialogMethod::Notify,
        Method::Options => InDialogMethod::Options,
        Method::Refer => InDialogMethod::Refer,
        _ => return None,
    })
}

#[cfg(test)]
mod tests;
