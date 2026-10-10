//! The dialog facts a leg learns from what it receives.

use super::{reliably, rseq_of, LegStack, ReliableProvisional};
use sip_message::generators::StackDialog;
use sip_message::{Method, SipRequest, SipResponse};

impl LegStack {
    /// Learn the dialog facts a response carries: the remote tag and the route
    /// set from a 1xx/2xx to a dialog-creating request alone (RFC 3261
    /// §12.1.2), the remote target from any non-final or 2xx (§12.2.1.2). A
    /// final to a CANCEL or a BYE, or a non-2xx final, states no dialog fact
    /// and leaves the dialog as it is; so does, once a 2xx confirmed the
    /// dialog, a response under another fork's tag (§13.2.2.4).
    pub fn learn_response(&mut self, response: &SipResponse) {
        let status = response.status();
        let other_dialog =
            self.confirmed && response.to().tag() != Some(self.dialog.remote_tag.as_str());
        if establishes_dialog(response) && !other_dialog {
            if let Some(tag) = response.to().tag() {
                self.dialog.remote_tag = tag.to_string();
            }
            let routes: Vec<String> = response
                .raw_text(sip_message::HeaderName::RecordRoute)
                .map(|s| s.to_string())
                .collect();
            if !routes.is_empty() {
                self.dialog.route_set = routes.into_iter().rev().collect();
            }
        }
        if status < 300 && !other_dialog {
            if let Some(contact) = response.contacts().as_slice().first() {
                self.dialog.remote_target = contact.uri().to_string();
            }
        }
        let mut rseq = None;
        if (101..200).contains(&response.status()) && reliably(response) {
            if let Some(seen) = rseq_of(response) {
                rseq = Some(seen);
                self.last_rseq = rseq;
                self.unacknowledged.push(ReliableProvisional {
                    rseq: seen,
                    cseq: response.cseq().seq(),
                    cseq_method: response.cseq().method().clone(),
                    to_tag: response.to().tag().map(str::to_string),
                });
            }
        }
        // A provisional carrying a To-tag opens an early dialog on this side
        // (RFC 3261 §12.1.2): the fork is the REMOTE's tag, and its target and
        // route set are the ones this provisional carried. Its local sequence
        // is seeded by the INVITE that created it (RFC 3261 §12.2.1.1) — never
        // the leg's running counter, which another fork's traffic may have
        // advanced by the time this fork rings.
        let provisional_fork =
            (101..200).contains(&response.status()) && *response.cseq().method() == Method::Invite;
        if let (true, Some(tag)) = (provisional_fork, response.to().tag().map(str::to_string)) {
            let seeded = self.sent_invite().map(|invite| invite.cseq().seq());
            let fork = StackDialog {
                local_cseq: seeded.unwrap_or(self.dialog.local_cseq),
                ..self.dialog.clone()
            };
            self.ring_early(&tag, fork, rseq);
        }
        // A 2xx to a PRACK or UPDATE an early dialog carried confirms nothing
        // (RFC 3261 §12.1, RFC 3262 §4, RFC 3311 §5.1): the fork keeps its own
        // sequence, and the INVITE's 2xx continues it.
        let answers_early_request =
            !self.confirmed && matches!(response.cseq().method(), Method::Prack | Method::Update);
        if response.status() >= 200 && response.status() < 300 && !answers_early_request {
            self.has_dialog = true;
            if let Some(tag) = response.to().tag() {
                let fork = self.early_dialog(tag).map(|early| early.dialog.local_cseq);
                if !self.confirmed && *response.cseq().method() == Method::Invite {
                    self.confirmed = true;
                    // The fork that answered is the dialog that confirmed, and
                    // the leg continues ITS sequence (RFC 3261 §12.2.1.1) —
                    // never a number another fork burned. A tag that rang no
                    // provisional confirms a dialog whose only prior request
                    // was the INVITE, so the INVITE's own CSeq seeds it.
                    let seeded = self.sent_invite().map(|invite| invite.cseq().seq());
                    self.dialog.local_cseq = fork.or(seeded).unwrap_or(self.dialog.local_cseq);
                } else {
                    // A non-confirming 2xx under a fork's tag syncs the leg to
                    // the numbers that fork has burned, so a later request the
                    // leg composes itself cannot reuse one of them.
                    self.dialog.local_cseq = self.dialog.local_cseq.max(fork.unwrap_or(0));
                }
            }
        }
    }

    /// Learn the dialog facts an inbound request carries, and keep it so a
    /// later response can answer it. The remote tag is learned only until a
    /// 2xx confirms the dialog, which fixes it (RFC 3261 §12.1.2); a request
    /// under another fork's tag then teaches the confirmed dialog nothing.
    pub fn learn_request(&mut self, request: &SipRequest) {
        let tag = request.from().tag().map(|t| t.to_string()).unwrap_or_default();
        if self.confirmed && !tag.is_empty() && tag != self.dialog.remote_tag {
            self.received.push(request.clone());
            return;
        }
        if !tag.is_empty() && !self.confirmed {
            self.dialog.remote_tag = tag;
        }
        // RFC 3261 §12.1.1 / §12.2.1.1 set the dialog's URIs once, from the
        // creating request; a request inside the dialog changes neither. The
        // display name and header parameters ride along so the scripted peer
        // writes its identity as the captured one did.
        if self.dialog.remote_uri.is_empty() {
            self.dialog.remote_uri = request.from().clone().without_tag().to_string();
        }
        if let Some(contact) = request.contacts().as_slice().first() {
            self.dialog.remote_target = contact.uri().to_string();
        }
        let routes: Vec<String> =
            request.raw_text(sip_message::HeaderName::RecordRoute).map(|s| s.to_string()).collect();
        if !routes.is_empty() {
            self.dialog.route_set = routes;
        }
        if request.method() == Method::Invite {
            self.dialog.call_id = request.call_id().to_string();
            if self.dialog.local_uri.is_empty() {
                self.dialog.local_uri = request.to().clone().without_tag().to_string();
            }
        }
        self.received.push(request.clone());
    }
}

/// Whether a response creates or confirms a dialog on the side that sent the
/// request: a 1xx (other than 100) or 2xx to an INVITE or SUBSCRIBE (RFC 3261
/// §12.1, RFC 6665 §4.1.2). Every other response states no dialog fact.
fn establishes_dialog(response: &SipResponse) -> bool {
    let status = response.status();
    let creating = matches!(response.cseq().method(), Method::Invite | Method::Subscribe);
    creating && (101..300).contains(&status)
}
