//! The SIP extensions an actor's UA implements: a request requiring another
//! is refused 420 naming it (RFC 3261 §8.2.2.3).

use sip_message::SipRequest;

/// The option tags an actor's UA implements (RFC 3262, RFC 4028, RFC 3891).
const UNDERSTOOD: &[&str] = &["100rel", "timer", "replaces"];

/// The tags of `req`'s `Require` this UA does not implement, comma-joined.
pub(super) fn unsupported_required(req: &SipRequest) -> Option<String> {
    let required = req.token_set::<sip_message::header::Require>()?.ok()?;
    let unknown: Vec<&str> =
        required.iter().filter(|t| !UNDERSTOOD.iter().any(|u| u.eq_ignore_ascii_case(t))).collect();
    (!unknown.is_empty()).then(|| unknown.join(", "))
}
