//! [`Wire`] — the single output buffer a render writes into.
//!
//! Every [`HeaderValue`](super::HeaderValue) appends to one buffer owned by the
//! caller, so assembling a message costs one allocation for the whole datagram
//! rather than one `String` per value.

/// An append-only UTF-8 byte buffer. Everything written goes through a `&str`
/// or an ASCII byte, so the contents are always valid UTF-8.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Wire {
    buf: Vec<u8>,
}

impl Wire {
    pub fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// A buffer pre-sized for the datagram about to be written — the reason a
    /// render is one allocation.
    pub fn with_capacity(bytes: usize) -> Self {
        Self { buf: Vec::with_capacity(bytes) }
    }

    pub fn str(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// Append one ASCII byte. Non-ASCII input would break the UTF-8 invariant,
    /// so it is dropped rather than written.
    pub fn byte(&mut self, b: u8) {
        if b.is_ascii() {
            self.buf.push(b);
        }
    }

    pub fn bytes(&mut self, b: &[u8]) {
        if b.is_ascii() {
            self.buf.extend_from_slice(b);
        }
    }

    /// Append a decimal number without formatting machinery.
    pub fn num(&mut self, mut n: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        self.buf.extend_from_slice(&digits[i..]);
    }

    /// Append `s` as an RFC 3261 quoted-string: wrapped in `"`, with `\` and
    /// `"` backslash-escaped so the value survives a re-parse unchanged.
    pub fn quoted(&mut self, s: &str) {
        self.buf.push(b'"');
        for chunk in s.split_inclusive(['\\', '"']) {
            match chunk.as_bytes().last() {
                Some(b'\\') | Some(b'"') => {
                    self.buf.extend_from_slice(&chunk.as_bytes()[..chunk.len() - 1]);
                    self.buf.push(b'\\');
                    self.buf.push(chunk.as_bytes()[chunk.len() - 1]);
                }
                _ => self.buf.extend_from_slice(chunk.as_bytes()),
            }
        }
        self.buf.push(b'"');
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// The buffer as text. Sound by construction — every write is UTF-8.
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.buf).unwrap_or("")
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_render_without_formatting() {
        let mut w = Wire::new();
        w.num(0);
        w.byte(b' ');
        w.num(70);
        w.byte(b' ');
        w.num(4294967295);
        assert_eq!(w.as_str(), "0 70 4294967295");
    }

    #[test]
    fn a_quoted_string_escapes_backslash_and_quote() {
        let mut w = Wire::new();
        w.quoted(r#"a"b\c"#);
        assert_eq!(w.as_str(), r#""a\"b\\c""#);
    }

    #[test]
    fn a_quoted_string_without_specials_is_wrapped_only() {
        let mut w = Wire::new();
        w.quoted("Alice");
        assert_eq!(w.as_str(), "\"Alice\"");
    }
}
