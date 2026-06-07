//! method — the one canonical SIP method type, owned by `sip-message` and shared
//! by every crate.
//!
//! Before this module a method was spelled three ways: the partial, overlapping
//! [`InDialogMethod`](crate::generators::InDialogMethod) /
//! [`OutOfDialogMethod`](crate::generators::OutOfDialogMethod) enums, and a bare
//! `String` on every message field (`SipRequest.method`, `CSeq.method`, …) plus
//! ~50 string comparisons across the workspace. [`Method`] collapses all of that
//! into a single representation parsed once at the wire.
//!
//! - Known methods fold case-insensitively to a canonical variant; their
//!   [`as_str`](Method::as_str) is always the RFC uppercase spelling.
//! - Any extension method the stack doesn't model is preserved verbatim as
//!   [`Method::Other`], so nothing is lost on the wire.
//! - The admissibility split (you may not send `ACK`/`CANCEL` as an ordinary
//!   in-dialog request; `REGISTER`/`SUBSCRIBE`/`PUBLISH` are out-of-dialog only)
//!   is preserved by keeping the two generator enums as *views* over `Method`
//!   (`TryFrom<&Method>` / `From<…> for Method`).

use std::convert::Infallible;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A SIP request method. The single canonical representation shared by every
/// crate; serializes as its wire token so replicated state stays string-shaped.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(from = "String", into = "String")]
pub enum Method {
    Invite,
    Ack,
    Bye,
    Cancel,
    Options,
    Register,
    Info,
    Update,
    Prack,
    Subscribe,
    Notify,
    Publish,
    Message,
    Refer,
    /// Any extension method not modelled natively. Preserves the original token.
    Other(String),
}

impl Method {
    /// The canonical wire spelling. Known methods are RFC uppercase; an
    /// [`Other`](Method::Other) returns its preserved token verbatim.
    pub fn as_str(&self) -> &str {
        match self {
            Method::Invite => "INVITE",
            Method::Ack => "ACK",
            Method::Bye => "BYE",
            Method::Cancel => "CANCEL",
            Method::Options => "OPTIONS",
            Method::Register => "REGISTER",
            Method::Info => "INFO",
            Method::Update => "UPDATE",
            Method::Prack => "PRACK",
            Method::Subscribe => "SUBSCRIBE",
            Method::Notify => "NOTIFY",
            Method::Publish => "PUBLISH",
            Method::Message => "MESSAGE",
            Method::Refer => "REFER",
            Method::Other(s) => s,
        }
    }

    /// Parse a wire method token. Known methods fold case-insensitively to their
    /// canonical variant; anything else is preserved verbatim as
    /// [`Other`](Method::Other) (case as given).
    pub fn from_wire(s: &str) -> Self {
        match s.to_ascii_uppercase().as_str() {
            "INVITE" => Method::Invite,
            "ACK" => Method::Ack,
            "BYE" => Method::Bye,
            "CANCEL" => Method::Cancel,
            "OPTIONS" => Method::Options,
            "REGISTER" => Method::Register,
            "INFO" => Method::Info,
            "UPDATE" => Method::Update,
            "PRACK" => Method::Prack,
            "SUBSCRIBE" => Method::Subscribe,
            "NOTIFY" => Method::Notify,
            "PUBLISH" => Method::Publish,
            "MESSAGE" => Method::Message,
            "REFER" => Method::Refer,
            _ => Method::Other(s.to_string()),
        }
    }

    /// RFC 3261 §12.1 — methods whose 2xx establishes a dialog (so the proxy
    /// Record-Routes them and the transaction layer treats them specially).
    pub fn is_dialog_creating(&self) -> bool {
        matches!(self, Method::Invite | Method::Subscribe)
    }
}

impl fmt::Display for Method {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Method {
    type Err = Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Method::from_wire(s))
    }
}

impl From<&str> for Method {
    fn from(s: &str) -> Self {
        Method::from_wire(s)
    }
}

impl From<String> for Method {
    fn from(s: String) -> Self {
        Method::from_wire(&s)
    }
}

impl From<&String> for Method {
    fn from(s: &String) -> Self {
        Method::from_wire(s)
    }
}

impl From<Method> for String {
    fn from(m: Method) -> Self {
        match m {
            Method::Other(s) => s,
            other => other.as_str().to_string(),
        }
    }
}

// Ergonomic, case-insensitive comparison against string literals so the existing
// `req.method == "INVITE"` call sites keep working (and become case-insensitive,
// matching the old `eq_ignore_ascii_case` checks) without a `Method::` rewrite.
impl PartialEq<str> for Method {
    fn eq(&self, other: &str) -> bool {
        self.as_str().eq_ignore_ascii_case(other)
    }
}

impl PartialEq<&str> for Method {
    fn eq(&self, other: &&str) -> bool {
        self.as_str().eq_ignore_ascii_case(other)
    }
}

impl PartialEq<String> for Method {
    fn eq(&self, other: &String) -> bool {
        self.as_str().eq_ignore_ascii_case(other)
    }
}

impl PartialEq<Method> for &str {
    fn eq(&self, other: &Method) -> bool {
        other.as_str().eq_ignore_ascii_case(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_methods_fold_case_insensitively() {
        assert_eq!(Method::from_wire("invite"), Method::Invite);
        assert_eq!(Method::from_wire("Invite"), Method::Invite);
        assert_eq!(Method::from_wire("INVITE"), Method::Invite);
        assert_eq!(Method::Invite.as_str(), "INVITE");
    }

    #[test]
    fn ack_and_cancel_are_first_class() {
        assert_eq!(Method::from_wire("ACK"), Method::Ack);
        assert_eq!(Method::from_wire("CANCEL"), Method::Cancel);
    }

    #[test]
    fn unknown_method_preserved_verbatim() {
        let m = Method::from_wire("FooBar");
        assert_eq!(m, Method::Other("FooBar".to_string()));
        assert_eq!(m.as_str(), "FooBar");
    }

    #[test]
    fn case_insensitive_str_compare() {
        assert!(Method::Invite == "invite");
        assert!(Method::Invite == "INVITE");
        assert!(Method::Bye != "INVITE");
    }

    #[test]
    fn dialog_creating_set() {
        assert!(Method::Invite.is_dialog_creating());
        assert!(Method::Subscribe.is_dialog_creating());
        assert!(!Method::Bye.is_dialog_creating());
        assert!(!Method::Refer.is_dialog_creating());
    }

    #[test]
    fn serde_roundtrips_as_wire_token() {
        let j = serde_json::to_string(&Method::Invite).unwrap();
        assert_eq!(j, "\"INVITE\"");
        let back: Method = serde_json::from_str("\"bye\"").unwrap();
        assert_eq!(back, Method::Bye);
        let ext: Method = serde_json::from_str("\"X-FOO\"").unwrap();
        assert_eq!(ext, Method::Other("X-FOO".to_string()));
    }
}
