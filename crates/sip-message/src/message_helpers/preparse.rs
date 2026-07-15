//! Strict pre-parse byte classifiers for ingress fast paths, decidable
//! before any SIP parse. Allocation-free, canonical-casing contracts.
//! Lenient raw-field *extraction* lives in [`crate::sniff`]; emergency
//! classification in [`super::emergency`].

/// Does the datagram start with the seven bytes `INVITE ` — i.e. a
/// new-INVITE request line? `false` for every other method and for
/// responses (`SIP/2.0 …`); a too-short buffer is trivially not an INVITE.
/// Case-SENSITIVE: method tokens are upper-case on the wire (RFC 3261 §7.1)
/// and this runs in the overload brake's hot path, which sheds only
/// canonical new-INVITEs.
pub fn is_invite_request_buffer(raw: &[u8]) -> bool {
    raw.starts_with(b"INVITE ")
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
