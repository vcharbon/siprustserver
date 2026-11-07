//! The `o=` line of a session description (RFC 4566 §5.2): its six fields
//! and the next version of the session it names.

/// The `o=<username> <sess-id> <sess-version> <nettype> <addrtype> <address>`
/// line lifted into its six fields (RFC 4566 §5.2), plus the description's
/// other lines so a consumer can ask whether anything BUT the origin changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpOrigin {
    pub username: String,
    pub session_id: String,
    pub session_version: u64,
    pub nettype: String,
    pub addrtype: String,
    pub unicast_address: String,
    /// The `o=` line as the wire spelled it, leading `o=` included.
    pub raw_origin_line: String,
    /// The description's lines with the `o=` line removed, newline-joined —
    /// what an "everything but the origin changed" comparison reads.
    pub body_excluding_origin: String,
}

/// The `o=` line of a session description, or `None` where the body is not a
/// `v=0` description (its first line is exactly `v=0`, RFC 4566 §5.1), states
/// no `o=` line before its first `m=` (§5 fixes the order), or spells one other
/// than the six fields of §5.2, one space apart, with a decimal sess-version.
/// An `o=` line that is no UTF-8 text reads as none.
pub fn parse_origin(body: &[u8]) -> Option<SdpOrigin> {
    let lines: Vec<&[u8]> =
        body.split(|&b| b == b'\n').map(|l| l.strip_suffix(b"\r").unwrap_or(l)).collect();
    if lines.first().copied() != Some(&b"v=0"[..]) {
        return None;
    }
    let (o_idx, o_line) = lines
        .iter()
        .enumerate()
        .take_while(|(_, l)| !l.starts_with(b"m="))
        .find(|(_, l)| l.starts_with(b"o="))?;
    let o_line = std::str::from_utf8(o_line).ok()?;
    let fields: Vec<&str> = o_line.get(2..)?.split(' ').collect();
    let [username, session_id, version, nettype, addrtype, unicast_address] = fields.as_slice()
    else {
        return None;
    };
    if fields.iter().any(|f| f.is_empty()) || !version.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let session_version = version.parse::<u64>().ok()?;
    let others = lines
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != o_idx)
        .map(|(_, l)| {
            crate::sdp_octets::decode(l).unwrap_or_else(|| String::from_utf8_lossy(l).into_owned())
        })
        .collect::<Vec<_>>()
        .join("\n");
    Some(SdpOrigin {
        username: username.to_string(),
        session_id: session_id.to_string(),
        session_version,
        nettype: nettype.to_string(),
        addrtype: addrtype.to_string(),
        unicast_address: unicast_address.to_string(),
        raw_origin_line: o_line.to_string(),
        body_excluding_origin: others,
    })
}

#[deny(clippy::arithmetic_side_effects)]
impl SdpOrigin {
    /// The five-field identity `o=` states about the SESSION — everything but
    /// the version, which is the revision counter (RFC 4566 §5.2). Two
    /// descriptions of one session agree on all five.
    pub fn identifies_same_session(&self, other: &SdpOrigin) -> bool {
        self.username == other.username
            && self.session_id == other.session_id
            && self.nettype == other.nettype
            && self.addrtype == other.addrtype
            && self.unicast_address == other.unicast_address
    }

    /// The `o=` line's value — everything after `o=`; the whole line where it
    /// does not open with `o=`.
    pub fn value(&self) -> &str {
        self.raw_origin_line.strip_prefix("o=").unwrap_or(&self.raw_origin_line)
    }

    /// The `o=` line a re-offer of THIS session states (RFC 3264 §8): the five
    /// identity fields unchanged, the version one above this one. `None` where
    /// the version is already the largest one this reader holds: no next
    /// version can be stated, and a wrapped one would run backwards.
    pub fn next_version_line(&self) -> Option<String> {
        Some(format!(
            "o={} {} {} {} {} {}",
            self.username,
            self.session_id,
            self.session_version.checked_add(1)?,
            self.nettype,
            self.addrtype,
            self.unicast_address
        ))
    }
}
