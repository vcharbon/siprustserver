//! Whether a header may be restated, by a party outside the transaction, on a
//! message the stack has already built — a routing decision's header
//! statements, for one — and whether such a party may state it on a message
//! the stack mints.

use super::class::HeaderClass;
use super::name::HeaderName;
use crate::write_check::{Position, WriteRefusal};

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
/// exchange or a dialog reference binds; the body binds every entity header
/// ([`crate::is_entity_header`]) besides. A compact form names its header
/// through the one compact-form table.
const BOUND: &[&str] = &[
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
    "Event",
    "Subscription-State",
    "Refer-To",
    "Session-Expires",
    "Min-SE",
];

/// Capability advertisements a mint narrows.
const ADVERTISED: &[&str] = &["Allow", "Supported", "Accept"];

/// The bound header a final's status is defined by, which the final's author
/// states: the challenge of a 401 and a 407 (RFC 3261 §21.4.2, §21.4.8), the
/// extensions a 420 refuses and a 421 demands (§21.4.15, §21.4.16), the
/// floor of a 422 (RFC 4028 §6). The author is no aggregating proxy (RFC 3261
/// §16.7 step 7): a 401 takes no `Proxy-Authenticate`, a 407 no `WWW-Authenticate`.
const DEFINING: &[(u16, &str)] = &[
    (401, "WWW-Authenticate"),
    (407, "Proxy-Authenticate"),
    (420, "Unsupported"),
    (421, "Require"),
    (422, "Min-SE"),
];

/// A message the stack mints, as a statement from outside the stack reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Minted {
    /// A request.
    Request,
    /// A final response of this status.
    Final(u16),
}

impl HeaderName {
    /// How far the header named `name` (any casing, compact or long form) may
    /// be restated on a built message.
    pub fn restatement_of(name: &str) -> Restatement {
        if Self::class_of(name) == HeaderClass::Structural
            || named(BOUND, name)
            || describes_body(name)
        {
            Restatement::Never
        } else if named(ADVERTISED, name) {
            Restatement::AtMint
        } else {
            Restatement::Free
        }
    }

    /// Whether `minted` takes a statement of `name` (any casing, compact or
    /// long form) from a party outside the stack, a decision's or a
    /// service's, set or removal: every name but a [`Restatement::Never`]
    /// one, and on a final the bound header its status is defined by (a
    /// 401's `WWW-Authenticate`, a 407's `Proxy-Authenticate`, a 420's
    /// `Unsupported`, a 421's `Require`, a 422's `Min-SE`). The refusal names
    /// the class.
    pub fn stated_at_mint(name: &str, minted: Minted) -> Result<(), WriteRefusal> {
        if Self::restatement_of(name) != Restatement::Never {
            return Ok(());
        }
        let class = if Self::class_of(name) == HeaderClass::Structural {
            "is the stack's own (RFC 3261 §16.6)"
        } else if matches!(minted, Minted::Final(status) if defines(status, name)) {
            return Ok(());
        } else {
            "is bound to the transaction, a negotiation, an authentication exchange, a dialog \
             reference or the body"
        };
        Err(WriteRefusal { position: Position::HeaderName, reason: format!("{name} {class}") })
    }
}

/// Whether `list` names the header `name` names.
fn named(list: &[&str], name: &str) -> bool {
    list.iter().any(|n| HeaderName::from(*n).matches(name))
}

/// Whether the header `name` names, a compact form read as its long name, is
/// an entity header.
fn describes_body(name: &str) -> bool {
    match HeaderName::known(name) {
        Some(known) => crate::is_entity_header(known.as_wire_str()),
        None => crate::is_entity_header(name),
    }
}

/// Whether a final of `status` is defined by the header `name` names.
fn defines(status: u16, name: &str) -> bool {
    DEFINING.iter().any(|(defined, n)| *defined == status && HeaderName::from(*n).matches(name))
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
            "Content-Type",
            "e",
            "Content-Transfer-Encoding",
            "content-location",
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

    #[test]
    fn a_mint_takes_every_statement_but_a_bound_or_structural_one() {
        for name in ["Allow", "k", "X-Custom", "Reason"] {
            assert_eq!(HeaderName::stated_at_mint(name, Minted::Request), Ok(()), "{name}");
            assert_eq!(HeaderName::stated_at_mint(name, Minted::Final(486)), Ok(()), "{name}");
        }
        let bound = HeaderName::stated_at_mint("require", Minted::Request).unwrap_err();
        assert_eq!(bound.position, Position::HeaderName);
        assert!(bound.reason.starts_with("require is bound"), "{}", bound.reason);
        let own = HeaderName::stated_at_mint("v", Minted::Final(401)).unwrap_err();
        assert!(own.reason.starts_with("v is the stack's own"), "{}", own.reason);
    }

    #[test]
    fn a_final_takes_the_bound_header_its_status_is_defined_by_and_no_other() {
        for (status, name) in [
            (401, "WWW-Authenticate"),
            (407, "proxy-authenticate"),
            (420, "Unsupported"),
            (421, "Require"),
            (422, "Min-SE"),
        ] {
            assert_eq!(HeaderName::stated_at_mint(name, Minted::Final(status)), Ok(()), "{name}");
            assert!(HeaderName::stated_at_mint(name, Minted::Final(403)).is_err(), "{name}");
            assert!(HeaderName::stated_at_mint(name, Minted::Request).is_err(), "{name}");
        }
        assert!(HeaderName::stated_at_mint("Proxy-Authenticate", Minted::Final(401)).is_err());
        assert!(HeaderName::stated_at_mint("Content-Length", Minted::Final(401)).is_err());
    }
}
