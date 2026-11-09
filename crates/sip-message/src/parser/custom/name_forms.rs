//! The header names of a datagram as the wire wrote them, compact forms
//! unexpanded: what a near-verbatim replay restates after the parser has
//! canonicalized every `SipHeader.name`.

use std::ops::ControlFlow::{Break, Continue};

use super::headers::read_header_name;
use super::scanner::{Scanner, COLON};
use crate::parser::SipParserLimits;

/// Scan the ordered **as-wire** header names from a raw datagram — RFC 3261
/// §7.3.3 compact forms are NOT expanded (`v` stays `v`). Mirrors
/// [`parse_headers`]'s line loop but reads names only (no value validation),
/// so name-scanning stays a single source of truth. Returns the names in wire
/// order aligned 1:1 with [`parse_headers`]'s `headers`; an empty Vec when the
/// head does not decode as the parser decodes it, or its start line or any
/// header line is malformed (the caller falls back to canonical names). Used
/// to recover the captured name-form for near-verbatim replay after the parser
/// has already canonicalized `SipHeader.name`.
///
/// [`parse_headers`]: super::headers::parse_headers
pub fn scan_header_name_forms(raw: &[u8], limits: &SipParserLimits) -> Vec<String> {
    let Ok(block) = super::scanner::header_block(raw) else {
        return Vec::new();
    };
    if block.bare_break.is_some() {
        return Vec::new();
    }
    let Some(head) = raw.get(..block.end) else {
        return Vec::new();
    };
    let (text, None) = super::decode_image(head) else {
        return Vec::new();
    };
    let image = text.as_str();
    let mut s = Scanner::new(image.as_bytes());
    if super::start_line::parse_start_line(&mut s, image, limits).is_err() {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    let read = s.walk(|s| {
        if s.at_end_of_headers() {
            s.consume_end_of_headers();
            return Break(true);
        }
        if s.at_end() {
            return Break(true);
        }
        let raw_name = match read_header_name(s) {
            Ok(n) if !n.is_empty() => n,
            _ => return Break(false),
        };
        if s.expect(COLON).is_err() {
            return Break(false);
        }
        s.skip_lws();
        let (Ok(_), Some(name)) = (s.read_header_value(), image.get(raw_name)) else {
            return Break(false);
        };
        names.push(name.to_string());
        Continue(())
    });
    if read != Ok(true) {
        return Vec::new();
    }
    names
}
