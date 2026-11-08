//! A datagram's transaction identity in one pass over its start line and
//! header block, borrowed from the datagram: no allocation, and no decoding
//! of anything past the headers. Each field reads the first row that carries
//! it, as the line scanners of [`sniff`](super) do; only the field's own
//! bytes need be UTF-8 (a field the datagram lacks, or holds in bytes that
//! are not UTF-8, is `None`), so a start line that is not UTF-8 leaves the
//! method and status `None` and the headers still read. Every reader of a
//! datagram's transaction and dialog keys (Call-ID, CSeq, top Via branch, To
//! tag, RSeq/RAck) can share one identity instead of scanning the datagram
//! again.

/// The identity fields of one datagram.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Identity<'a> {
    pub call_id: Option<&'a str>,
    pub cseq: Option<u32>,
    /// The CSeq method token.
    pub cseq_method: Option<&'a str>,
    /// The request method (start line), `None` for a response.
    pub method: Option<&'a str>,
    /// The status code, `None` for a request.
    pub status: Option<u16>,
    /// The RSeq of a provisional response above 100 (RFC 3262 §7.1).
    pub rseq: Option<u64>,
    /// A provisional response above 100 lists `100rel` in a Require header:
    /// it is sent reliably (RFC 3262 §3).
    pub reliable: bool,
    /// The RAck response-num of a PRACK: the RSeq it acknowledges (RFC 3262
    /// §7.2).
    pub rack_rseq: Option<u64>,
    /// The To `tag` parameter.
    pub to_tag: Option<&'a str>,
    /// The `branch` parameter of the top Via (RFC 3261 §17 transaction key),
    /// read as [`via_branch`](super::via_branch) reads it.
    pub branch: Option<&'a str>,
}

/// The identity of `raw`. The header block ends at its first empty line;
/// header names match case-insensitively, compact forms included (`i`, `t`,
/// `v`).
pub fn identity(raw: &[u8]) -> Identity<'_> {
    let mut lines = raw.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let mut id = Identity::default();
    let Some(start) = lines.next() else { return id };
    if let Ok(start) = std::str::from_utf8(start) {
        let mut words = start.split_ascii_whitespace();
        let first = words.next();
        // The SIP-Version compares ignoring case (RFC 3261 §7.1).
        if first.is_some_and(|w| w.eq_ignore_ascii_case("SIP/2.0")) {
            id.status = words.next().and_then(|s| s.parse().ok());
        } else {
            id.method = first;
        }
    }
    let provisional = id.status.is_some_and(|s| (101..200).contains(&s));
    let prack = id.method == Some("PRACK");
    let (mut cseq, mut rseq, mut rack, mut to, mut via) = (false, false, false, false, false);
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let name = trim(line.get(..colon).unwrap_or_default());
        let value = trim(line.get(colon + 1..).unwrap_or_default());
        let is = |names: &[&[u8]]| names.iter().any(|n| name.eq_ignore_ascii_case(n));
        if id.call_id.is_none() && is(&[b"call-id", b"i"]) {
            id.call_id = text(value);
        } else if !cseq && is(&[b"cseq"]) {
            cseq = true;
            let mut words = words(value);
            id.cseq = words.next().and_then(number);
            id.cseq_method = words.next().and_then(text);
        } else if provisional && !rseq && is(&[b"rseq"]) {
            rseq = true;
            id.rseq = number(value);
        } else if provisional && is(&[b"require"]) {
            id.reliable |=
                value.split(|&b| b == b',').any(|t| trim(t).eq_ignore_ascii_case(b"100rel"));
        } else if prack && !rack && is(&[b"rack"]) {
            rack = true;
            id.rack_rseq = words(value).next().and_then(number);
        } else if !to && is(&[b"to", b"t"]) {
            to = true;
            id.to_tag = tag(value);
        } else if !via && is(&[b"via", b"v"]) {
            via = true;
            id.branch = branch_of(value);
        }
    }
    id
}

/// `b` as text, when it is UTF-8.
fn text(b: &[u8]) -> Option<&str> {
    std::str::from_utf8(b).ok()
}

/// `b` as a decimal number.
fn number<N: std::str::FromStr>(b: &[u8]) -> Option<N> {
    text(b)?.parse().ok()
}

/// The words of `b`, split at ASCII whitespace.
fn words(b: &[u8]) -> impl Iterator<Item = &[u8]> {
    b.split(u8::is_ascii_whitespace).filter(|w| !w.is_empty())
}

/// The `branch` parameter of one Via row's value, `None` when empty or absent:
/// the bytes after the first `branch=` up to `;`, `,`, a space or a tab.
pub(super) fn branch_of(value: &[u8]) -> Option<&str> {
    let at = value.windows(b"branch=".len()).position(|w| w == b"branch=")?;
    let rest = value.get(at + b"branch=".len()..)?;
    let end = rest.iter().position(|b| matches!(b, b';' | b',' | b' ' | b'\t'));
    let b = trim(rest.get(..end.unwrap_or(rest.len()))?);
    text(b).filter(|b| !b.is_empty())
}

fn trim(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map_or(start, |e| e + 1);
    b.get(start..end).unwrap_or(&[])
}

/// The `tag` parameter of a To value, outside its angle brackets.
fn tag(value: &[u8]) -> Option<&str> {
    let params = match value.iter().rposition(|&b| b == b'>') {
        Some(gt) => value.get(gt + 1..)?,
        None => value,
    };
    params.split(|&b| b == b';').skip(1).find_map(|p| {
        let eq = p.iter().position(|&b| b == b'=')?;
        let (k, v) = (p.get(..eq)?, p.get(eq + 1..)?);
        trim(k).eq_ignore_ascii_case(b"tag").then(|| text(trim(v))).flatten()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reliable_provisional_has_its_whole_identity() {
        let raw = b"SIP/2.0 183 Session Progress\r\nVia: SIP/2.0/UDP h;branch=z9hG4bK1\r\n\
            To: <sip:bob@h>;tag=b1;x=y\r\nFrom: <sip:a@h>;tag=a1\r\ni: c1@h\r\n\
            cseq: 7 INVITE\r\nRSeq: 3\r\n\r\nv=0\r\nTo: no\r\n";
        assert_eq!(
            identity(raw),
            Identity {
                call_id: Some("c1@h"),
                cseq: Some(7),
                cseq_method: Some("INVITE"),
                method: None,
                status: Some(183),
                rseq: Some(3),
                reliable: false,
                rack_rseq: None,
                to_tag: Some("b1"),
                branch: Some("z9hG4bK1"),
            }
        );
    }

    #[test]
    fn a_reliable_provisional_lists_100rel_in_any_require_row() {
        let raw = b"SIP/2.0 180 Ringing\r\nRequire: timer\r\nRequire: foo, 100REL\r\n\
            RSeq: 1\r\nCSeq: 1 INVITE\r\n\r\n";
        assert!(identity(raw).reliable);
        let final_ = b"SIP/2.0 200 OK\r\nRequire: 100rel\r\nCSeq: 1 INVITE\r\n\r\n";
        assert!(!identity(final_).reliable, "only a provisional above 100 is reliable");
    }

    #[test]
    fn a_prack_has_the_rseq_it_acknowledges() {
        let raw = b"PRACK sip:b@h SIP/2.0\r\nCSeq: 2 PRACK\r\nRAck: 4 1 INVITE\r\n\r\n";
        assert_eq!(identity(raw).rack_rseq, Some(4));
        let bye = b"BYE sip:b@h SIP/2.0\r\nRAck: 4 1 INVITE\r\n\r\n";
        assert_eq!(identity(bye).rack_rseq, None, "only a PRACK acknowledges");
    }

    #[test]
    fn the_branch_is_the_top_vias() {
        let raw = b"BYE sip:b@h SIP/2.0\r\nv: SIP/2.0/UDP a;rport;branch=z9hG4bKtop, \
            SIP/2.0/UDP b;branch=z9hG4bKfold\r\nVia: SIP/2.0/UDP c;branch=z9hG4bKlow\r\n\r\n";
        assert_eq!(identity(raw).branch, Some("z9hG4bKtop"));
        let none = b"BYE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP a\r\nVia: SIP/2.0/UDP c;branch=z9hG4bKlow\r\n\r\n";
        assert_eq!(identity(none).branch, None, "a top Via without a branch has none");
    }

    #[test]
    fn a_request_has_its_method_and_to_tag_and_no_rseq() {
        let raw =
            b"BYE sip:bob@h SIP/2.0\nCall-ID: c2\nTo: <sip:bob@h>;tag=b1\nCSeq: 2 BYE\nRSeq: 9\n\n";
        let id = identity(raw);
        assert_eq!(
            (id.method, id.status, id.cseq, id.cseq_method),
            (Some("BYE"), None, Some(2), Some("BYE"))
        );
        assert_eq!((id.call_id, id.to_tag, id.rseq), (Some("c2"), Some("b1"), None));
    }

    #[test]
    fn a_100_has_no_rseq_and_an_untagged_to_has_no_tag() {
        let raw = b"SIP/2.0 100 Trying\r\nTo: <sip:bob@h;tag=uri-param>\r\nRSeq: 1\r\n\r\n";
        let id = identity(raw);
        assert_eq!((id.status, id.rseq, id.to_tag), (Some(100), None, None));
    }

    #[test]
    fn the_first_row_of_each_field_wins() {
        let raw = b"SIP/2.0 183 Progress\r\nCSeq: 1 INVITE\r\nCSeq: 2 BYE\r\n\
            To: <sip:b@h>;tag=first\r\nt: <sip:b@h>;tag=second\r\nRSeq: 4\r\nRSeq: 5\r\n\
            Call-ID: c1\r\ni: c2\r\n\r\n";
        let id = identity(raw);
        assert_eq!((id.cseq, id.cseq_method, id.to_tag), (Some(1), Some("INVITE"), Some("first")));
        assert_eq!((id.rseq, id.call_id), (Some(4), Some("c1")));
    }

    #[test]
    fn a_field_needs_only_its_own_bytes_in_utf8() {
        let raw = b"BYE sip:b@h SIP/2.0\r\nVia: SIP/2.0/UDP \xffh;branch=z9hG4bKtop\r\n\
            Via: SIP/2.0/UDP c;branch=z9hG4bKlow\r\nTo: \"\xfe\" <sip:b@h>;tag=b1\r\n\
            Call-ID: c1\r\n\r\n";
        let id = identity(raw);
        assert_eq!(id.branch, Some("z9hG4bKtop"), "the top Via row, whatever else it holds");
        assert_eq!(crate::sniff::via_branch(raw).as_deref(), id.branch);
        assert_eq!(id.to_tag, Some("b1"));
        let start = b"BYE sip:\xff@h SIP/2.0\r\nCall-ID: c1\r\nCSeq: 2 BYE\r\n\r\n";
        let id = identity(start);
        assert_eq!((id.method, id.status), (None, None), "an unreadable start line");
        assert_eq!((id.call_id, id.cseq), (Some("c1"), Some(2)), "the headers still read");
    }

    #[test]
    fn garbage_is_an_empty_identity_never_a_panic() {
        for raw in [&b""[..], b"\xff\xfe", b"SIP/2.0", b"SIP/2.0 abc\r\nCSeq:\r\n:\r\n", b"\n\n\n"]
        {
            let id = identity(raw);
            assert_eq!((id.call_id, id.cseq, id.status), (None, None, None), "{raw:?}");
        }
    }
}
