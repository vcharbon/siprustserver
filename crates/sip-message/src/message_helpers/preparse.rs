//! Strict pre-parse byte classifiers for ingress fast paths: request-line
//! method check and To-tag presence, decidable before any SIP parse.
//! Allocation-free, canonical-casing contracts. Lenient raw-field
//! *extraction* lives in [`crate::sniff`]; emergency classification in
//! [`super::emergency`].

use super::bytes::find_subslice;

/// Does the datagram start with the seven bytes `INVITE ` — i.e. a
/// new-INVITE request line? `false` for every other method and for
/// responses (`SIP/2.0 …`); a too-short buffer is trivially not an INVITE.
/// Case-SENSITIVE: method tokens are upper-case on the wire (RFC 3261 §7.1)
/// and this runs in the overload brake's hot path, which sheds only
/// canonical new-INVITEs.
pub fn is_invite_request_buffer(raw: &[u8]) -> bool {
    raw.starts_with(b"INVITE ")
}

/// Cheap byte scan: does the datagram carry a `To`-tag (`;tag=` on the `To`
/// header line)? The pre-parse discriminator between an **initial** request
/// (no `To`-tag — a fresh INVITE the brake may shed) and an **in-dialog**
/// one (`To`-tag present — ACK / BYE / re-INVITE the dispatcher fast-paths).
///
/// Walks the header section line by line (CRLF-delimited) until it finds the
/// `To` header, then scans that **one physical line** for `;tag=`:
///   - the `To` header must sit at a **line start** (so a `;tag=` inside,
///     say, a `History-Info` value cannot be mistaken for the dialog tag),
///     matched case-SENSITIVELY against the canonical `To`/`t` (RFC 3261
///     §7.1 compact form) followed by `:` or SP;
///   - the scan stops at the **first** `To` line it reaches: a tagless first
///     `To` yields `false` even if later lines are not inspected (a
///     well-formed message has exactly one `To`);
///   - the walk ends at the blank line that terminates the header section
///     (`false`), and at an unterminated final line (`false`) — a `To`-tag
///     must be on a properly CRLF-terminated `To` line to count.
pub fn buffer_has_to_tag(raw: &[u8]) -> bool {
    let mut idx = 0;
    while idx < raw.len() {
        let line_start = idx;
        // No CRLF from here on → no terminated line left to inspect.
        let line_end = match find_subslice(&raw[line_start..], b"\r\n") {
            Some(rel) => line_start + rel,
            None => return false,
        };
        // A zero-length line is the blank line that ends the header section.
        if line_end == line_start {
            return false;
        }

        // Match a `To`/`t` header at the line start. The checked lookahead
        // (`raw.get`) keeps the compare in bounds on a truncated buffer.
        let c0 = raw[line_start];
        let is_to_line = if c0 == b'T' && raw.get(line_start + 1) == Some(&b'o') {
            matches!(raw.get(line_start + 2), Some(&b':') | Some(&b' '))
        } else if c0 == b't' {
            matches!(raw.get(line_start + 1), Some(&b':') | Some(&b' '))
        } else {
            false
        };
        if is_to_line {
            // Confine the `;tag=` scan to this one physical `To` line.
            return find_subslice(&raw[line_start..line_end], b";tag=").is_some();
        }

        idx = line_end + 2;
    }
    false
}

#[cfg(test)]
mod invite_buffer_tests {
    //! Pins [`is_invite_request_buffer`]: the exact seven-byte `INVITE `
    //! prefix, case-sensitive, `false` for every other method / response /
    //! short buffer.

    use super::is_invite_request_buffer;

    #[test]
    fn invite_request_line_is_an_invite() {
        let raw = b"INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\nVia: SIP/2.0/UDP x\r\n\r\n";
        assert!(is_invite_request_buffer(raw));
    }

    #[test]
    fn bare_invite_space_is_the_minimal_match() {
        assert!(is_invite_request_buffer(b"INVITE "));
    }

    #[test]
    fn other_methods_are_not_invites() {
        for line in [
            &b"OPTIONS sip:bob SIP/2.0\r\n"[..],
            &b"ACK sip:bob SIP/2.0\r\n"[..],
            &b"BYE sip:bob SIP/2.0\r\n"[..],
            &b"CANCEL sip:bob SIP/2.0\r\n"[..],
            &b"REGISTER sip:bob SIP/2.0\r\n"[..],
        ] {
            assert!(!is_invite_request_buffer(line), "{line:?} must not be an INVITE");
        }
    }

    #[test]
    fn response_is_not_an_invite_request() {
        assert!(!is_invite_request_buffer(b"SIP/2.0 200 OK\r\n"));
    }

    #[test]
    fn invite_without_trailing_space_does_not_match() {
        // "INVITEX ..." is not the INVITE method token — the trailing space is
        // load-bearing (it is byte 7 of the compare).
        assert!(!is_invite_request_buffer(b"INVITEX sip:bob SIP/2.0\r\n"));
    }

    #[test]
    fn lower_case_invite_does_not_match() {
        assert!(!is_invite_request_buffer(b"invite sip:bob SIP/2.0\r\n"));
    }

    #[test]
    fn too_short_buffers_are_not_invites() {
        assert!(!is_invite_request_buffer(b""));
        assert!(!is_invite_request_buffer(b"INVITE"));
    }
}

#[cfg(test)]
mod to_tag_tests {
    //! Pins the byte-scan contract of [`buffer_has_to_tag`]: line-start `To`
    //! match (canonical + compact form, `:` or SP separator), scan confined
    //! to the first `To` line, and every walk-termination edge (blank line,
    //! missing CRLF, truncated buffer).

    use super::buffer_has_to_tag;

    /// Build a minimal INVITE datagram whose `To` header line is exactly `to`
    /// (passed without the trailing CRLF, which this helper appends).
    fn invite_with_to_line(to: &str) -> Vec<u8> {
        format!(
            "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-tt\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
{to}\r\n\
Call-ID: tt-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn to_tag_present_is_in_dialog() {
        let buf = invite_with_to_line("To: <sip:bob@b2bua.test>;tag=bob-dialog-tag");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn no_to_tag_is_initial_request() {
        let buf = invite_with_to_line("To: <sip:bob@b2bua.test>");
        assert!(!buffer_has_to_tag(&buf));
    }

    #[test]
    fn compact_to_form_with_colon_is_recognised() {
        let buf = invite_with_to_line("t: <sip:bob@b2bua.test>;tag=compact-colon");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn compact_to_form_with_space_is_recognised() {
        let buf = invite_with_to_line("t <sip:bob@b2bua.test>;tag=compact-space");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn full_to_form_with_space_separator_is_recognised() {
        let buf = invite_with_to_line("To <sip:bob@b2bua.test>;tag=full-space");
        assert!(buffer_has_to_tag(&buf));
    }

    #[test]
    fn tag_inside_a_non_to_header_does_not_count() {
        // A `;tag=` embedded in a non-`To` header (here History-Info) must NOT
        // be mistaken for the dialog tag — the To must be at a line start. The
        // actual `To` line carries no tag, so the result is `false`.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-hi\r\n\
History-Info: <sip:carol@x>;index=1;tag=not-a-dialog-tag\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: hi-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_to_tag(raw.as_bytes()));
    }

    #[test]
    fn to_header_name_match_is_case_sensitive() {
        // Canonical casing required: a lower-cased `to:` (not the compact `t`
        // form either) is not matched as the To header, so its `;tag=` is
        // never inspected ⇒ `false`.
        let buf = invite_with_to_line("to: <sip:bob@b2bua.test>;tag=lowercased");
        assert!(!buffer_has_to_tag(&buf));
    }

    #[test]
    fn first_to_line_decides_even_when_tagless() {
        // The scan returns on the FIRST To line it reaches: a tagless To yields
        // `false` even though a (malformed) second To line carries a tag.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-dup\r\n\
To: <sip:bob@b2bua.test>\r\n\
To: <sip:bob@b2bua.test>;tag=second-to-tag\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
Call-ID: dup-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        assert!(!buffer_has_to_tag(raw.as_bytes()));
    }

    #[test]
    fn tag_only_in_body_after_header_terminator_does_not_count() {
        // The walk stops at the blank line ending the header section, so a
        // `To: ...;tag=` that appears only in the *body* is never reached.
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-body\r\n\
From: <sip:alice@flooder.test>;tag=alice-tag\r\n\
Call-ID: body-call@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Type: text/plain\r\n\
Content-Length: 28\r\n\r\n\
To: <sip:x@y>;tag=in-the-body";
        assert!(!buffer_has_to_tag(raw.as_bytes()));
    }

    #[test]
    fn buffer_without_crlf_is_not_in_dialog() {
        // No CRLF anywhere ⇒ `false`, even though a `;tag=` is present — a
        // To-tag must be on a properly CRLF-terminated To line.
        assert!(!buffer_has_to_tag(b"To: <sip:bob>;tag=no-crlf"));
    }

    #[test]
    fn unterminated_to_line_at_end_is_not_in_dialog() {
        // The To line is the final line with no trailing CRLF: the walk
        // reaches it but finds no line terminator ⇒ `false`.
        let raw = b"INVITE sip:bob SIP/2.0\r\n\
Via: SIP/2.0/UDP h;branch=z9hG4bK-u\r\n\
To: <sip:bob@b2bua.test>;tag=unterminated";
        assert!(!buffer_has_to_tag(raw));
    }

    #[test]
    fn empty_buffer_is_not_in_dialog() {
        assert!(!buffer_has_to_tag(b""));
    }

    #[test]
    fn blank_line_first_is_not_in_dialog() {
        // A leading CRLF makes the very first line zero-length (the header
        // terminator) ⇒ `false` immediately.
        assert!(!buffer_has_to_tag(b"\r\nTo: <sip:bob>;tag=after-blank\r\n\r\n"));
    }

    #[test]
    fn to_at_buffer_end_without_separator_does_not_panic() {
        // A truncated buffer ending exactly at "To" / "t" exercises the
        // bounds-checked lookahead. A CRLF-terminated bare "To" line carries
        // no `;tag=` ⇒ `false`.
        assert!(!buffer_has_to_tag(b"To\r\n"));
        assert!(!buffer_has_to_tag(b"t\r\n"));
        assert!(!buffer_has_to_tag(b"To"));
        assert!(!buffer_has_to_tag(b"T"));
    }
}
