//! The User-to-User payload (RFC 7433), in the `User-to-User` header or the
//! escaped `?User-to-User=` URI header: an `encoding=hex` one as bytes, the
//! ASCII digit runs a payload carries (a dial form behind a one-byte
//! protocol discriminator, typically; an `ascii`/`text` payload is its own
//! bytes), and the hex rewrite through a byte substitution that keeps the
//! width — same-width fakes keep the hex the
//! same length. A byte the substitution leaves keeps its own hex characters;
//! a rewritten one takes the case the payload was written in. A hex payload
//! that decodes to no text ([`is_text`]) is binary the anonymiser does not
//! understand, redacted to zeros ([`redacted`]).

/// The decoded payload; `None` unless the text is an even number of hex
/// digits.
pub fn decode(hex: &str) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) || !hex.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).ok()).collect()
}

/// Whether a decoded payload is text: printable ASCII and line breaks behind
/// an optional one-byte protocol discriminator. Anything else is a binary
/// payload the anonymiser does not understand.
pub fn is_text(bytes: &[u8]) -> bool {
    let printable = |b: &u8| (0x20..0x7f).contains(b) || matches!(b, b'\r' | b'\n' | b'\t');
    let body =
        bytes.get(1..).filter(|_| bytes.first().is_some_and(|b| !printable(b))).unwrap_or(bytes);
    !body.is_empty() && body.iter().all(printable)
}

/// The payload redacted: every hex digit `0`, its width kept.
pub fn redacted(hex: &str) -> String {
    "0".repeat(hex.len())
}

/// Every maximal ASCII digit run of the decoded payload, in order.
pub fn digit_runs(hex: &str) -> Vec<String> {
    decode(hex).map(|bytes| text_digit_runs_of(&bytes)).unwrap_or_default()
}

/// Every maximal ASCII digit run of an `ascii`/`text` payload — the text is
/// the bytes.
pub fn text_digit_runs(text: &str) -> Vec<String> {
    text_digit_runs_of(text.as_bytes())
}

fn text_digit_runs_of(bytes: &[u8]) -> Vec<String> {
    bytes
        .split(|b| !b.is_ascii_digit())
        .filter(|run| !run.is_empty())
        .map(|run| String::from_utf8(run.to_vec()).expect("digits are ASCII"))
        .collect()
}

/// The payload with its decoded bytes passed through `rw` and re-encoded;
/// `None` when the text is not hex or `rw` changed the width.
pub fn rewrite(hex: &str, rw: impl Fn(&[u8]) -> Vec<u8>) -> Option<String> {
    let bytes = decode(hex)?;
    let rewritten = rw(&bytes);
    if rewritten.len() != bytes.len() {
        return None;
    }
    let upper = !hex.bytes().any(|b| b.is_ascii_lowercase());
    let mut out = String::with_capacity(hex.len());
    for (i, (before, after)) in bytes.iter().zip(&rewritten).enumerate() {
        if before == after {
            out.push_str(&hex[2 * i..2 * i + 2]);
        } else if upper {
            out.push_str(&format!("{after:02X}"));
        } else {
            out.push_str(&format!("{after:02x}"));
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hex_payload_decodes_lists_its_digit_runs_and_rewrites_in_its_own_case() {
        // `\x04` then a national number, then a label.
        let hex = "04303232333435363738e2f3";
        assert_eq!(decode(hex).unwrap(), b"\x04022345678\xe2\xf3");
        let ascii = "043032323334353637384142";
        assert_eq!(digit_runs(hex), vec!["022345678"]);
        assert_eq!(digit_runs("04303132AB3334"), vec!["012", "34"]);
        assert_eq!(decode("0430"), Some(vec![4, 48]));
        assert_eq!(decode("043"), None);
        assert_eq!(decode("04zz"), None);
        assert_eq!(decode(""), None);
        assert!(digit_runs("xyz").is_empty());
        assert_eq!(text_digit_runs("ab00835123456789cd|2210"), vec!["00835123456789", "2210"]);

        let swap =
            |b: &[u8]| String::from_utf8_lossy(b).replace("22345678", "00098273").into_bytes();
        assert_eq!(rewrite(ascii, swap).unwrap(), "043030303039383237334142");
        // Untouched bytes keep their own characters; rewritten ones follow
        // the payload's case (upper unless a lowercase letter is present).
        let upper = "04303232333435363738E2F3";
        let out = rewrite(upper, |b| {
            let mut v = b.to_vec();
            v[1] = b'\xab';
            v
        })
        .unwrap();
        assert_eq!(out, "04AB3232333435363738E2F3");
        assert_eq!(rewrite(hex, |b| b[1..].to_vec()), None, "a width change is refused");
        assert_eq!(rewrite("nothex", <[u8]>::to_vec), None);
    }
}
