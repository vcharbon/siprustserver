//! The PASSporT payload of an `Identity` header (RFC 8224 §4, RFC 8225): the
//! base64url JWS payload segment as bytes and back, the string leaves of its
//! JSON claims (the `orig`/`dest` `tn` numbers among them), and its rewrite
//! through a byte substitution that keeps the width — same-width fakes keep
//! the JSON and therefore the segment the same length. The header and
//! signature segments are the caller's to leave alone.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;

/// The decoded payload and the count of `=` padding bytes the segment
/// carried (RFC 8224 writes none; a padded one is put back as found).
/// `None` when the segment is not base64url.
pub fn decode(segment: &str) -> Option<(Vec<u8>, usize)> {
    let unpadded = segment.trim_end_matches('=');
    let bytes = URL_SAFE_NO_PAD.decode(unpadded).ok()?;
    Some((bytes, segment.len() - unpadded.len()))
}

/// The segment of `bytes`, padded with `pad` `=`.
pub fn encode(bytes: &[u8], pad: usize) -> String {
    let mut out = URL_SAFE_NO_PAD.encode(bytes);
    out.extend(std::iter::repeat_n('=', pad));
    out
}

/// Every string leaf of the payload's JSON claims, in document order —
/// `orig.tn`, each `dest.tn`, `origid`, `attest`, whatever else is a string.
/// Empty when the segment does not decode to JSON.
pub fn strings(segment: &str) -> Vec<String> {
    let Some((bytes, _)) = decode(segment) else { return Vec::new() };
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    collect_strings(&value, &mut out);
    out
}

fn collect_strings(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => items.iter().for_each(|v| collect_strings(v, out)),
        serde_json::Value::Object(map) => map.values().for_each(|v| collect_strings(v, out)),
        _ => {}
    }
}

/// The segment with its decoded bytes passed through `rw` and re-encoded;
/// `None` when the segment does not decode.
pub fn rewrite(segment: &str, rw: impl Fn(&[u8]) -> Vec<u8>) -> Option<String> {
    let (bytes, pad) = decode(segment)?;
    Some(encode(&rw(&bytes), pad))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAYLOAD: &str = "eyJhdHRlc3QiOiJBIiwiZGVzdCI6eyJ0biI6WyIrMTU1NTIzNDU2NzgiXX0sImlhdCI6MTQ0MzIwODM0NSwib3JpZyI6eyJ0biI6IisxNTU1MjIzNDU2NyJ9LCJvcmlnaWQiOiJkZTMwNWQ1NC03NWI0LTQzMWItYWRiMi1lYjZiOWU1NDYwMTQifQ";

    #[test]
    fn a_payload_round_trips_and_lists_its_string_leaves() {
        let (bytes, pad) = decode(PAYLOAD).unwrap();
        assert_eq!(pad, 0);
        assert!(bytes.starts_with(b"{\"attest\":\"A\""));
        assert_eq!(encode(&bytes, 0), PAYLOAD);
        assert_eq!(
            strings(PAYLOAD),
            vec!["A", "+15552345678", "+15552234567", "de305d54-75b4-431b-adb2-eb6b9e546014"]
        );
        // Padding is put back as found; a non-base64url or non-JSON segment
        // has no leaves.
        let padded = format!("{PAYLOAD}==");
        assert_eq!(decode(&padded).unwrap().1, 2);
        assert_eq!(rewrite(&padded, <[u8]>::to_vec).unwrap(), padded);
        assert!(strings("not*base64").is_empty());
        assert!(strings(&encode(b"plain text", 0)).is_empty());
        assert_eq!(decode("not*base64"), None);
    }

    #[test]
    fn a_same_width_rewrite_keeps_the_segment_width() {
        let rewritten = rewrite(PAYLOAD, |b| {
            String::from_utf8(b.to_vec()).unwrap().replace("5552345678", "0009123456").into_bytes()
        })
        .unwrap();
        assert_eq!(rewritten.len(), PAYLOAD.len());
        assert_ne!(rewritten, PAYLOAD);
        assert_eq!(strings(&rewritten)[1], "+10009123456");
    }
}
