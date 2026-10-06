//! The one Contact statement policy. The stack states its own `Contact` ONLY
//! where the header establishes a dialog or refreshes the dialog target
//! (RFC 3261 §8.1.1.8, §12.1.1, §12.2.1.1). A 3xx / 485 names retry targets
//! that only their author knows: the decision's Contacts on a redirect the
//! stack builds, the peer's Contact set on one it relays. Anywhere else the
//! header would name nothing a peer may use, so no generator or relay states one.

use crate::header::HeaderName;
use crate::method::Method;
use crate::parser::custom::contact_entries::{
    contact_entry_spans, read_contact_entry, ContactEntry,
};
use crate::types::SipHeader;

/// Whether a request of `method` states the sender's Contact: the
/// dialog-establishing / target-refresh methods — INVITE, initial and re-INVITE
/// (RFC 3261 §12.2.2), UPDATE (RFC 3311 §5.1), SUBSCRIBE / NOTIFY / REFER
/// (RFC 6665 §4.4.1, RFC 3515 §2.4.2), REGISTER (§10.2, the binding itself).
/// ACK, BYE, CANCEL, PRACK, OPTIONS, INFO and MESSAGE refresh no target, so
/// they state none.
pub fn request_states_contact(method: &Method) -> bool {
    matches!(
        method,
        Method::Invite
            | Method::Update
            | Method::Subscribe
            | Method::Notify
            | Method::Refer
            | Method::Register
    )
}

/// Whether a response of `status` answering a `cseq_method` request states a
/// Contact: retry targets ([`response_names_retry_targets`]), the bindings a
/// 2xx to REGISTER lists (§10.3), and otherwise where
/// [`response_states_own_contact`] does.
pub fn response_states_contact(cseq_method: &Method, status: u16) -> bool {
    response_names_retry_targets(status)
        || (*cseq_method == Method::Register && matches!(status, 200..=299))
        || response_states_own_contact(cseq_method, status)
}

/// Whether a response states the responder's OWN Contact as the dialog's
/// remote target: a tagged 1xx / a 2xx answering a dialog-establishing or
/// target-refresh request (§12.1.1 on a dialog-establishing 2xx, RFC 3311 §5.2
/// on the 2xx to UPDATE, RFC 6665 §4.2.1 on the 202 to REFER). A 1xx names a
/// dialog only under a To tag; a caller that may send an untagged 1xx checks
/// the tag. REGISTER makes no dialog; a 3xx / 485 names targets the responder
/// does not own.
pub fn response_states_own_contact(cseq_method: &Method, status: u16) -> bool {
    request_states_contact(cseq_method)
        && *cseq_method != Method::Register
        && matches!(status, 101..=299)
}

/// Whether a response of `status` names retry targets in its Contact set: a
/// 3xx's redirect targets (§8.1.3.4, §21.3) or a 485's alternates (§21.4.22).
pub fn response_names_retry_targets(status: u16) -> bool {
    matches!(status, 300..=399 | 485)
}

/// The readable Contact entries of `headers`, in order, when a response of
/// `status` names retry targets with them; none otherwise, where the Contact
/// is the responder's own and a relay states its own in its place. Each entry
/// is read as the parser reads it (`contact_entries`) and rides verbatim, its
/// parameters kept, on a line of its own; a `*` (a REGISTER-only wildcard,
/// §10.2.2) or an unreadable entry is dropped: a relay forwards no target it
/// cannot read.
pub fn retry_targets(status: u16, headers: &[SipHeader]) -> Vec<SipHeader> {
    if !response_names_retry_targets(status) {
        return Vec::new();
    }
    headers
        .iter()
        .filter(|h| HeaderName::Contact.matches(&h.name))
        .flat_map(|h| {
            contact_entry_spans(&h.value)
                .filter(|span| matches!(read_contact_entry(span), ContactEntry::Readable(_)))
                .map(|value| SipHeader { name: h.name.clone(), value })
        })
        .collect()
}

/// Drop every Contact line of `headers` unless a response of `status` names
/// retry targets: a final the stack states for itself never carries another
/// response's targets.
pub fn retain_retry_targets_for(status: u16, headers: &mut Vec<SipHeader>) {
    if !response_names_retry_targets(status) {
        headers.retain(|h| !HeaderName::Contact.matches(&h.name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_refresh_requests_state_contact() {
        for m in [
            Method::Invite,
            Method::Update,
            Method::Subscribe,
            Method::Notify,
            Method::Refer,
            Method::Register,
        ] {
            assert!(request_states_contact(&m), "{m:?}");
        }
    }

    #[test]
    fn non_refresh_requests_state_none() {
        for m in [
            Method::Ack,
            Method::Bye,
            Method::Cancel,
            Method::Prack,
            Method::Options,
            Method::Info,
            Method::Message,
        ] {
            assert!(!request_states_contact(&m), "{m:?}");
        }
    }

    #[test]
    fn dialog_establishing_responses_state_contact() {
        assert!(response_states_contact(&Method::Invite, 180));
        assert!(response_states_contact(&Method::Invite, 200));
        assert!(response_states_contact(&Method::Update, 200));
    }

    #[test]
    fn target_refresh_and_subscription_answers_state_the_own_contact() {
        assert!(response_states_own_contact(&Method::Update, 200));
        assert!(response_states_own_contact(&Method::Refer, 202));
        assert!(response_states_own_contact(&Method::Invite, 183));
        assert!(!response_states_own_contact(&Method::Update, 491));
        assert!(!response_states_own_contact(&Method::Refer, 400));
        assert!(!response_states_own_contact(&Method::Info, 200));
    }

    #[test]
    fn redirect_targets_are_not_the_own_contact() {
        assert!(!response_states_own_contact(&Method::Invite, 302));
        assert!(!response_states_own_contact(&Method::Invite, 485));
    }

    #[test]
    fn a_register_answer_lists_bindings_not_an_own_contact() {
        assert!(!response_states_own_contact(&Method::Register, 200));
        assert!(response_states_contact(&Method::Register, 200));
    }

    fn header(name: &str, value: &str) -> SipHeader {
        SipHeader { name: crate::SipStr::owned(name), value: crate::SipStr::owned(value) }
    }

    #[test]
    fn retry_targets_are_the_contact_lines_of_a_3xx_or_485_only() {
        let headers = vec![
            header("Contact", "<sip:carol@192.0.2.9>"),
            header("Warning", "399 example \"moved\""),
            header("m", "<sip:dave@192.0.2.10>"),
        ];
        let values = |hs: Vec<SipHeader>| -> Vec<String> {
            hs.iter().map(|h| h.value.as_str().to_string()).collect()
        };
        assert_eq!(
            values(retry_targets(302, &headers)),
            ["<sip:carol@192.0.2.9>", "<sip:dave@192.0.2.10>"]
        );
        assert_eq!(values(retry_targets(485, &headers)).len(), 2);
        assert!(retry_targets(486, &headers).is_empty());
        assert!(retry_targets(200, &headers).is_empty());
    }

    /// Each entry is read as the lenient parser reads it: a wildcard and an
    /// entry the strict URI gate refuses are dropped, every readable entry
    /// rides verbatim on a line of its own, its parameters kept.
    #[test]
    fn retry_targets_keep_the_readable_entries_only() {
        let headers = vec![
            header("Contact", "*"),
            header("Contact", "<sip:@>"),
            header("Contact", "<sip:carol@192.0.2.9>, <sip:@>"),
            header("Contact", "<sip:dave@192.0.2.10>;q=0.5;expires=60, <sip:bob@192.0.2.2:99999>"),
            header("Contact", "*, <sip:erin@192.0.2.11>"),
        ];
        let kept: Vec<String> =
            retry_targets(302, &headers).iter().map(|h| h.value.as_str().to_string()).collect();
        assert_eq!(
            kept,
            [
                "<sip:carol@192.0.2.9>",
                "<sip:dave@192.0.2.10>;q=0.5;expires=60",
                "<sip:erin@192.0.2.11>"
            ]
        );
    }

    #[test]
    fn a_final_outside_the_retry_set_keeps_no_contact_line() {
        let base = vec![header("Contact", "<sip:carol@192.0.2.9>"), header("Warning", "399 x")];
        let mut redirect = base.clone();
        retain_retry_targets_for(302, &mut redirect);
        assert_eq!(redirect.len(), 2);
        let mut refusal = base;
        retain_retry_targets_for(480, &mut refusal);
        assert_eq!(refusal.len(), 1);
        assert_eq!(refusal[0].name.as_str(), "Warning");
    }

    #[test]
    fn redirects_state_contact_for_every_method() {
        assert!(response_states_contact(&Method::Options, 302));
        assert!(response_states_contact(&Method::Invite, 485));
    }

    #[test]
    fn non_refresh_finals_state_none() {
        assert!(!response_states_contact(&Method::Prack, 200));
        assert!(!response_states_contact(&Method::Options, 200));
        assert!(!response_states_contact(&Method::Bye, 200));
        assert!(!response_states_contact(&Method::Info, 200));
        assert!(!response_states_contact(&Method::Message, 200));
        assert!(!response_states_contact(&Method::Cancel, 200));
        assert!(!response_states_contact(&Method::Invite, 100));
        assert!(!response_states_contact(&Method::Invite, 486));
    }
}
