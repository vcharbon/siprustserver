//! RFC 4028 on a message the back-to-back UA mints toward a leg: whether that
//! message offers the session timer, and withdrawing the negotiation from one
//! that takes no part in it.

use crate::header::{self, HeaderName, HeaderValue};
use crate::types::SipHeader;

/// The option tag naming the session timer extension (RFC 4028 §3).
const TIMER: &str = "timer";

/// True iff `headers` offer the session timer: their `Supported` lines, read
/// as one set (RFC 3261 §7.3.1), name `timer` (RFC 4028 §7.1).
pub fn offers_session_timer(headers: &[SipHeader]) -> bool {
    headers
        .iter()
        .filter(|h| HeaderName::Supported.matches(&h.name))
        .filter_map(|h| header::Supported::parse(&h.value).ok())
        .any(|set| set.contains(TIMER))
}

/// Withdraw the session-timer negotiation from a minted message whose leg
/// takes no part in it: `Session-Expires` and `Min-SE` go, and each `Require`
/// line loses its `timer` tag, a line left naming nothing going with it. Every
/// other line stays byte-identical.
pub fn withdraw_session_timer(headers: &mut Vec<SipHeader>) {
    headers.retain(|h| {
        !HeaderName::SessionExpires.matches(&h.name) && !HeaderName::MinSe.matches(&h.name)
    });
    let mut kept = Vec::with_capacity(headers.len());
    for h in headers.drain(..) {
        if !HeaderName::Require.matches(&h.name) {
            kept.push(h);
            continue;
        }
        let Ok(required) = header::Require::parse(&h.value) else {
            kept.push(h);
            continue;
        };
        if !required.contains(TIMER) {
            kept.push(h);
            continue;
        }
        let rest = required.without(TIMER);
        if !rest.is_empty() {
            kept.push(SipHeader { name: h.name, value: rest.to_wire().into() });
        }
    }
    *headers = kept;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(name: &str, value: &str) -> SipHeader {
        SipHeader { name: name.to_string().into(), value: value.to_string().into() }
    }

    fn pairs(headers: &[SipHeader]) -> Vec<(String, String)> {
        headers.iter().map(|h| (h.name.to_string(), h.value.to_string())).collect()
    }

    #[test]
    fn the_offer_is_the_timer_tag_in_any_supported_line() {
        assert!(offers_session_timer(&[hdr("Supported", "100rel"), hdr("supported", "Timer")]));
        assert!(!offers_session_timer(&[hdr("Supported", "100rel"), hdr("Require", "timer")]));
        assert!(!offers_session_timer(&[]));
    }

    /// The interval and floor go, `Require` keeps every other tag, and lines
    /// that say nothing about the timer are untouched.
    #[test]
    fn withdrawing_leaves_every_other_line_as_it_was() {
        let mut headers = vec![
            hdr("Session-Expires", "1800;refresher=uac"),
            hdr("Require", "timer"),
            hdr("Require", "100rel, timer"),
            hdr("Require", "precondition"),
            hdr("Min-SE", "90"),
            hdr("Supported", "timer"),
            hdr("X-Vendor-Thing", "kept"),
        ];
        withdraw_session_timer(&mut headers);
        assert_eq!(
            pairs(&headers),
            vec![
                ("Require".to_string(), "100rel".to_string()),
                ("Require".to_string(), "precondition".to_string()),
                ("Supported".to_string(), "timer".to_string()),
                ("X-Vendor-Thing".to_string(), "kept".to_string()),
            ]
        );
    }
}
