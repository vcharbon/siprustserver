//! Percent-codec for the B2BUA's correlation params (Via `cr`/`lg`, Contact
//! `callRef`/`leg` — values contain `|`/`@`/`:`, unsafe in a SIP param).
//! The single home for this codec so the encoder and its inverse cannot
//! drift across crates: the B2BUA stamps; sip-txn and the router read.

/// Percent-encode all but RFC 3986 unreserved characters.
pub fn encode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

/// Inverse of [`encode_param`]; invalid/truncated escapes pass through verbatim.
pub fn decode_param(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'a' + (v - 10)) as char,
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{decode_param, encode_param};

    #[test]
    fn param_round_trips_through_unsafe_chars() {
        let raw = "w0|alice@example.com:5060|ab12cd34";
        let enc = encode_param(raw);
        assert!(!enc.contains('|') && !enc.contains('@') && !enc.contains(':'));
        assert_eq!(decode_param(&enc), raw, "encode∘decode is identity");
    }

    #[test]
    fn decode_passes_truncated_escapes_verbatim() {
        assert_eq!(decode_param("ab%"), "ab%");
        assert_eq!(decode_param("ab%4"), "ab%4");
        assert_eq!(decode_param("%zz"), "%zz");
    }
}
