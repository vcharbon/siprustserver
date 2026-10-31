//! A datagram's transaction identity in one pass over its start line and
//! header block, borrowed from the datagram: no allocation, and no decoding
//! of anything past the headers. A field the datagram lacks, or holds in
//! bytes that are not UTF-8, is `None`. Every reader of a datagram's
//! transaction and dialog keys (Call-ID, CSeq, top Via branch, To tag,
//! RSeq/RAck) can share one identity instead of scanning the datagram again.

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
/// header names match case-insensitively, compact forms included (`i`, `t`).
pub fn identity(raw: &[u8]) -> Identity<'_> {
    let mut lines = raw.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l));
    let mut id = Identity::default();
    let Some(start) = lines.next().and_then(|l| std::str::from_utf8(l).ok()) else {
        return id;
    };
    let mut words = start.split_ascii_whitespace();
    let first = words.next();
    if first == Some("SIP/2.0") {
        id.status = words.next().and_then(|s| s.parse().ok());
    } else {
        id.method = first;
    }
    let provisional = id.status.is_some_and(|s| (101..200).contains(&s));
    let prack = id.method == Some("PRACK");
    let mut top_via = false;
    for line in lines {
        if line.is_empty() {
            break;
        }
        let Some(colon) = line.iter().position(|&b| b == b':') else { continue };
        let name = trim(line.get(..colon).unwrap_or_default());
        let value = line.get(colon + 1..).unwrap_or_default();
        let Ok(value) = std::str::from_utf8(value).map(str::trim) else { continue };
        if name.eq_ignore_ascii_case(b"call-id") || name.eq_ignore_ascii_case(b"i") {
            id.call_id.get_or_insert(value);
        } else if name.eq_ignore_ascii_case(b"cseq") {
            let mut parts = value.split_ascii_whitespace();
            id.cseq = parts.next().and_then(|n| n.parse().ok());
            id.cseq_method = parts.next();
        } else if provisional && name.eq_ignore_ascii_case(b"rseq") {
            id.rseq = value.parse().ok();
        } else if provisional && name.eq_ignore_ascii_case(b"require") {
            id.reliable |= value.split(',').any(|t| t.trim().eq_ignore_ascii_case("100rel"));
        } else if prack && name.eq_ignore_ascii_case(b"rack") {
            id.rack_rseq = value.split_ascii_whitespace().next().and_then(|n| n.parse().ok());
        } else if name.eq_ignore_ascii_case(b"to") || name.eq_ignore_ascii_case(b"t") {
            id.to_tag = tag(value);
        } else if !top_via && (name.eq_ignore_ascii_case(b"via") || name.eq_ignore_ascii_case(b"v"))
        {
            top_via = true;
            id.branch = branch_of(value);
        }
    }
    id
}

/// The `branch` parameter of one Via row's value, `None` when empty or absent.
pub(super) fn branch_of(value: &str) -> Option<&str> {
    let rest = value.get(value.find("branch=")? + "branch=".len()..)?;
    let b = super::up_to_delimiter(rest, &[';', ',', ' ', '\t']).trim();
    (!b.is_empty()).then_some(b)
}

fn trim(b: &[u8]) -> &[u8] {
    let start = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(b.len());
    let end = b.iter().rposition(|c| !c.is_ascii_whitespace()).map_or(start, |e| e + 1);
    b.get(start..end).unwrap_or(&[])
}

/// The `tag` parameter of a To value, outside its angle brackets.
fn tag(value: &str) -> Option<&str> {
    let params = match value.rfind('>') {
        Some(gt) => value.get(gt + 1..)?,
        None => value,
    };
    params.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("tag").then(|| v.trim())
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
    fn garbage_is_an_empty_identity_never_a_panic() {
        for raw in [&b""[..], b"\xff\xfe", b"SIP/2.0", b"SIP/2.0 abc\r\nCSeq:\r\n:\r\n", b"\n\n\n"]
        {
            let id = identity(raw);
            assert_eq!((id.call_id, id.cseq, id.status), (None, None, None), "{raw:?}");
        }
    }
}
