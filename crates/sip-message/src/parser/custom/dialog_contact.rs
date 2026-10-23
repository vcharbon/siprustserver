//! The Contact of a message that can establish a dialog names the dialog's
//! remote target (RFC 3261 §12.1.1, §12.1.2), so it is a SIP or SIPS URI
//! (§8.1.1.8 for the request, §12.1.1 for the 1xx / 2xx answering it). A
//! Contact of another scheme stays readable everywhere else (a REGISTER
//! binding, a 3xx alternative, an OPTIONS answer).

use crate::error::SipParseError;
use crate::types::ContactSet;

/// The methods whose request can establish a dialog: INVITE (RFC 3261),
/// SUBSCRIBE and NOTIFY (RFC 6665 §4.4.1), REFER (RFC 3515 §2.4.4).
pub(super) fn can_establish_dialog(method: &str) -> bool {
    ["INVITE", "SUBSCRIBE", "NOTIFY", "REFER"].iter().any(|m| m.eq_ignore_ascii_case(method))
}

/// Why the Contact set of a dialog-establishing message, `what` naming it in
/// the reason, names no dialog target, if one of its URIs is not SIP or SIPS.
pub(super) fn dialog_contact_refusal(contacts: &ContactSet, what: &str) -> Option<SipParseError> {
    let ContactSet::Contacts(contacts) = contacts else { return None };
    let other = contacts.iter().map(|c| c.uri()).find(|u| !matches!(u.scheme(), "sip" | "sips"))?;
    Some(SipParseError::new(format!(
        "{what} carries a Contact of scheme \"{}\": a dialog's remote target is a SIP or SIPS \
         URI (RFC 3261 §8.1.1.8, §12.1.1): \"{other}\"",
        other.scheme()
    )))
}
