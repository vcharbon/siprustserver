//! A datagram's transaction identity in one pass over its start line and
//! header block, borrowed from the datagram: no allocation, and no decoding
//! of anything past the headers. A field the datagram lacks, or holds in
//! bytes that are not UTF-8, is `None`.

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
    /// The To `tag` parameter of a response.
    pub to_tag: Option<&'a str>,
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
        } else if id.status.is_some()
            && (name.eq_ignore_ascii_case(b"to") || name.eq_ignore_ascii_case(b"t"))
        {
            id.to_tag = tag(value);
        }
    }
    id
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
                to_tag: Some("b1"),
            }
        );
    }

    #[test]
    fn a_request_has_its_method_and_no_tag_or_rseq() {
        let raw =
            b"BYE sip:bob@h SIP/2.0\nCall-ID: c2\nTo: <sip:bob@h>;tag=b1\nCSeq: 2 BYE\nRSeq: 9\n\n";
        let id = identity(raw);
        assert_eq!(
            (id.method, id.status, id.cseq, id.cseq_method),
            (Some("BYE"), None, Some(2), Some("BYE"))
        );
        assert_eq!((id.call_id, id.to_tag, id.rseq), (Some("c2"), None, None));
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
