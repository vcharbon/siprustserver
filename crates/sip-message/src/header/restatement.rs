//! Whether a header may be restated, by a party outside the transaction, on a
//! message the stack has already built — a routing decision's header
//! statements, for one.

use super::class::HeaderClass;
use super::name::HeaderName;

/// How far a header's value may be restated on a built message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Restatement {
    /// Any message may carry a restated value.
    Free,
    /// Only where the message is minted: the value is a capability
    /// advertisement the mint narrows (`Allow`, `Supported`, `Accept`), so a
    /// restatement after it would undo the narrowing.
    AtMint,
    /// Never: the value is the stack's, or is bound to the transaction, the
    /// offer/answer or subscription negotiation, or the entity the message
    /// carries.
    Never,
}

/// Headers whose value the transaction, a negotiation, an authentication
/// exchange, a dialog reference or the body binds, with the compact forms
/// RFC 3261 §7.3.3, RFC 3265 and RFC 3515 / RFC 4028 give them.
const BOUND: &[&str] = &[
    "c",
    "e",
    "o",
    "r",
    "u",
    "x",
    "Content-Language",
    "Content-ID",
    "Proxy-Require",
    "Unsupported",
    "Authorization",
    "Proxy-Authorization",
    "WWW-Authenticate",
    "Proxy-Authenticate",
    "Authentication-Info",
    "Replaces",
    "Join",
    "Timestamp",
    "Allow-Events",
    "Require",
    "RSeq",
    "RAck",
    "Content-Type",
    "Content-Encoding",
    "Content-Disposition",
    "MIME-Version",
    "Event",
    "Subscription-State",
    "Refer-To",
    "Session-Expires",
    "Min-SE",
];

/// Capability advertisements a mint narrows (`k` is `Supported`'s compact form).
const ADVERTISED: &[&str] = &["Allow", "Supported", "k", "Accept"];

impl HeaderName {
    /// How far the header named `name` (any casing, compact or long form) may
    /// be restated on a built message.
    pub fn restatement_of(name: &str) -> Restatement {
        let named = |list: &[&str]| list.iter().any(|n| HeaderName::from(*n).matches(name));
        if Self::class_of(name) == HeaderClass::Structural || named(BOUND) {
            Restatement::Never
        } else if named(ADVERTISED) {
            Restatement::AtMint
        } else {
            Restatement::Free
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_and_structural_names_are_never_restated() {
        for name in [
            "Via",
            "Contact",
            "CSeq",
            "Content-Length",
            "require",
            "RSeq",
            "RAck",
            "c",
            "Content-Disposition",
            "MIME-Version",
            "o",
            "Subscription-State",
            "r",
            "x",
            "Min-SE",
            "Content-Language",
            "Content-ID",
            "Proxy-Require",
            "Unsupported",
            "Authorization",
            "Proxy-Authorization",
            "WWW-Authenticate",
            "Proxy-Authenticate",
            "Authentication-Info",
            "Replaces",
            "Join",
            "Timestamp",
            "Allow-Events",
            "u",
        ] {
            assert_eq!(HeaderName::restatement_of(name), Restatement::Never, "{name}");
        }
    }

    #[test]
    fn advertisements_are_restated_at_the_mint_only_and_the_rest_freely() {
        for name in ["Allow", "k", "accept"] {
            assert_eq!(HeaderName::restatement_of(name), Restatement::AtMint, "{name}");
        }
        for name in ["P-Charging-Vector", "Privacy", "Reason", "X-Custom", "User-to-User"] {
            assert_eq!(HeaderName::restatement_of(name), Restatement::Free, "{name}");
        }
    }
}
