//! The responses a leg composes to the requests it received.

use super::{creates_dialog, frozen, media_type, reliably, rseq_of, Answer, LegStack, StackError};
use sip_message::generators::{generate_response, GenerateResponseOpts, StackDialog};
use sip_message::{Method, SipHeader, SipRequest, SipResponse, SipStr};
use sip_template::TemplateHeader;

impl LegStack {
    /// Compose a response to the newest received request whose CSeq method is
    /// `cseq_method`, or to the newest received request when none is named.
    ///
    /// **Which dialog it answers under.** A request already carrying a To-tag
    /// names its dialog and the response repeats it (RFC 3261 §12.1.1);
    /// otherwise `early_tag` — the fork the step rides (§6.1) — answers, and a
    /// leg with no fork answers under its own tag. A 2xx INVITE final under a
    /// fork's tag is the dialog that CONFIRMED, so the leg adopts it: the
    /// requests that follow ride the dialog that was answered, not the one the
    /// leg opened with.
    ///
    /// **What it advertises.** Only what the document states. A generated
    /// response relays no capability set of its own, because RFC 3261 §20.5 /
    /// §20.37 read an absent `Allow` / `Supported` as "no information given" —
    /// the only truthful thing to say about a set no capture held.
    pub fn respond(
        &mut self,
        answer: &Answer<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<SipResponse, StackError> {
        self.respond_selected(answer, headers, body, content_type, |_| true)
    }

    /// [`respond`](Self::respond) to ONE named server transaction: the request
    /// with this CSeq number on the dialog `to_tag` names — two forks number
    /// their first in-dialog request alike, so the tag is part of the name.
    pub fn respond_to(
        &mut self,
        cseq: u32,
        to_tag: Option<&str>,
        answer: &Answer<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
    ) -> Result<SipResponse, StackError> {
        self.respond_selected(answer, headers, body, content_type, |r| {
            r.cseq().seq() == cseq && r.to().tag() == to_tag
        })
    }

    pub(super) fn respond_selected(
        &mut self,
        answer: &Answer<'_>,
        headers: &[TemplateHeader],
        body: Vec<u8>,
        content_type: Option<String>,
        selected: impl Fn(&SipRequest) -> bool,
    ) -> Result<SipResponse, StackError> {
        let Answer { status, reason, cseq_method, early_tag } = *answer;
        let wanted = cseq_method.map(Method::from_wire);
        let matching = || {
            self.received
                .iter()
                .rev()
                .filter(|r| wanted.as_ref().is_none_or(|m| r.cseq().method() == m))
                .filter(|r| selected(r))
        };
        // The newest transaction of that method still OPEN, and only then the
        // newest at all. A capture may answer two outstanding transactions of
        // one method out of order — a PRACK ladder is where it happens — and
        // recency alone would stamp the second response with the transaction
        // the first already ended, leaving the older one unanswered for the
        // whole run.
        let request = matching()
            .find(|r| !self.finalled.contains(&r.cseq().seq()))
            .or_else(|| matching().next())
            .cloned()
            .ok_or(StackError::NoRequest { leg: self.leg.clone(), status })?;
        let states_contact =
            sip_message::generators::response_states_contact(request.cseq().method(), status);
        let mut extra = frozen(headers);
        let to_tag = request
            .to()
            .tag()
            .map(str::to_string)
            .or_else(|| early_tag.map(str::to_string))
            .unwrap_or_else(|| self.dialog.local_tag.clone());
        if (101..200).contains(&status) && *request.cseq().method() == Method::Invite {
            self.pace_rseq(&to_tag, &mut extra);
        }
        let opts = GenerateResponseOpts {
            to_tag: Some(to_tag.clone()),
            contact: states_contact.then(|| self.contact()),
            body,
            content_type: content_type.as_deref().map(media_type),
            extra_headers: extra,
            incoming_source: None,
        };
        let response = generate_response(&request, status, reason, &opts);
        let mut rseq = None;
        if (101..200).contains(&status) && reliably(&response) {
            rseq = Some(
                rseq_of(&response).ok_or(StackError::NoRSeq { leg: self.leg.clone(), status })?,
            );
            self.last_rseq = rseq;
        }
        // A provisional under a To-tag opens an early dialog on this side
        // (RFC 3261 §12.1.1): the fork answers under that tag, on the dialog
        // the INVITE established, in its own CSeq space.
        let invite = request.cseq().method() == Method::Invite;
        let key = (request.call_id().to_string(), request.cseq().seq());
        if (101..200).contains(&status) && invite {
            let fork =
                StackDialog { local_tag: to_tag.clone(), local_cseq: 0, ..self.dialog.clone() };
            self.ring_early(&to_tag, key.clone(), fork, rseq);
        }
        if status >= 200 {
            self.finalled.insert(request.cseq().seq());
        }
        // A non-2xx final to an INVITE creates no dialog and ends the early
        // dialogs its provisionals opened (RFC 3261 §12.3).
        if status >= 300 && invite {
            self.end_early(&key);
        }
        // Only a 2xx to a dialog-creating request opens the leg's dialog (RFC
        // 3261 §12.1.1).
        if (200..300).contains(&status) && creates_dialog(request.cseq().method()) {
            self.has_dialog = true;
        }
        if (200..300).contains(&status) && invite {
            // The fork that answers IS the dialog that confirmed, so the leg
            // takes its tag and continues that fork's OWN sequence (RFC 3261
            // §12.2.1.1) — never a number another fork burned.
            let fork = self.early_dialog(&to_tag).map(|e| e.dialog.local_cseq);
            self.dialog.local_tag = to_tag;
            if !self.confirmed {
                self.confirmed = true;
                self.dialog.local_cseq = fork.unwrap_or(self.dialog.local_cseq);
            } else {
                self.dialog.local_cseq = self.dialog.local_cseq.max(fork.unwrap_or(0));
            }
        }
        Ok(response)
    }

    /// Number the `RSeq` the document states into this fork's own sequence.
    ///
    /// RFC 3262 §3 gives every early dialog ONE space: its first reliable
    /// provisional seeds it and each later one is greater by exactly one, so a
    /// document that states an unrelated number on the second is renumbered
    /// onto the space it rides.
    /// A number the document repeats retransmits its provisional and stands.
    pub(super) fn pace_rseq(&self, tag: &str, extra: &mut [SipHeader]) {
        let Some(last) = self.early_rseq(tag) else { return };
        for header in extra.iter_mut().filter(|h| sip_message::HeaderName::RSeq.matches(&h.name)) {
            let stated = header.value.as_str().trim().parse::<u32>().ok();
            if stated.is_some_and(|stated| stated != last) {
                header.value = SipStr::owned(&last.saturating_add(1).to_string());
            }
        }
    }
}

/// The reason phrase a status carries when the document states none. A document
/// that states one wins; this is only the fallback for a status the flow
/// invented.
pub(crate) fn reason_for(status: u16) -> &'static str {
    match status {
        100 => "Trying",
        180 => "Ringing",
        183 => "Session Progress",
        200 => "OK",
        202 => "Accepted",
        404 => "Not Found",
        480 => "Temporarily Unavailable",
        481 => "Call/Transaction Does Not Exist",
        486 => "Busy Here",
        487 => "Request Terminated",
        488 => "Not Acceptable Here",
        491 => "Request Pending",
        603 => "Decline",
        _ => "Response",
    }
}

#[cfg(test)]
mod tests;
