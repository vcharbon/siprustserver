//! The one reader of Contact entries (RFC 3261 §20.10). The parser's Contact
//! set and a relay's retry targets read each entry the same way, so a target
//! one keeps the other keeps, and one drops the other drops.

use super::structured_headers::{
    parse_contact, top_level_comma_entries, validate_strict_sip_uri, ParsedContact,
};
use crate::sip_str::SipStr;

/// How one top-level entry of a Contact line reads.
pub enum ContactEntry {
    /// `*`, the REGISTER-only wildcard (§10.2.2).
    Wildcard,
    /// A name-addr / addr-spec whose URI passes the strict gate.
    Readable(ParsedContact),
    /// An entry whose URI the strict gate refuses, and why.
    Unreadable { contact: ParsedContact, reason: String },
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
    let contact = parse_contact(entry);
    match validate_strict_sip_uri(&contact.uri) {
        None => ContactEntry::Readable(contact),
        Some(reason) => ContactEntry::Unreadable { contact, reason },
    }
}
