//! The one reader of Contact entries (RFC 3261 §20.10). The parser's Contact
//! set and a relay's retry targets read each entry the same way, so a target
//! one keeps the other keeps, and one drops the other drops.

use super::structured_headers::{parse_contact, top_level_comma_entries};
use crate::error::SipParseError;
use crate::header::{Contact, HeaderValue, NameAddr, Uri};
use crate::sip_str::SipStr;

/// How one top-level entry of a Contact line reads.
pub enum ContactEntry {
    /// `*`, the REGISTER-only wildcard (§10.2.2).
    Wildcard,
    /// A name-addr / addr-spec the strict name-addr reader takes.
    Readable(Box<Contact>),
    /// An entry the strict name-addr reader refuses, and why.
    Unreadable { reason: String },
}

/// The non-empty top-level entries of one Contact line, each a verbatim span
/// of it, parameters included.
pub fn contact_entry_spans(line: &SipStr) -> impl Iterator<Item = SipStr> + '_ {
    top_level_comma_entries(line.as_str())
        .filter(|seg| !seg.is_empty())
        .map(|seg| line.reslice(seg))
}

/// Read one entry span of [`contact_entry_spans`].
pub fn read_contact_entry(entry: &SipStr) -> ContactEntry {
    if entry.as_str() == "*" {
        return ContactEntry::Wildcard;
    }
    match Contact::parse(entry) {
        Ok(contact) => ContactEntry::Readable(Box::new(contact)),
        Err(SipParseError { reason, .. }) => ContactEntry::Unreadable { reason },
    }
}

/// The lenient reading of an entry the strict reader refuses, for a message
/// held to no wire grammar: its parts as scanned, its URI held
/// [`Uri::opaque`] when the URI reader refuses it.
pub fn read_contact_entry_lenient(entry: &SipStr) -> Contact {
    let p = parse_contact(entry);
    Contact::new(NameAddr::from_parts(p.display_name, Uri::parse_or_verbatim(&p.uri), p.params))
}
