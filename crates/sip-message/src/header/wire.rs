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

    /// Append one grammar literal — a separator such as `;`, `=`, `<` or a
    /// space. Every call site writes an ASCII literal, so non-ASCII is
    /// unreachable by construction; it is asserted in debug and dropped in
    /// release rather than breaking the UTF-8 invariant [`as_str`](Self::as_str)
    /// relies on.
    pub fn byte(&mut self, b: u8) {
        debug_assert!(b.is_ascii(), "Wire::byte takes ASCII grammar literals, got {b:#04x}");
        if b.is_ascii() {
            self.buf.push(b);
        }
    }

    /// Append a decimal number.
    pub fn num(&mut self, n: u64) {
        use std::io::Write as _;
        // Writing into a `Vec` cannot fail.
        let _ = write!(self.buf, "{n}");
    }

    /// Append `s` as an RFC 3261 quoted-string: wrapped in `"`, with `\`, `"`
    /// and every control byte `qdtext` excludes backslash-escaped as a
    /// `quoted-pair`, so the value survives a re-parse unchanged. A CR or LF
    /// fits neither and is written as given: the freeze refuses the line.
    pub fn quoted(&mut self, s: &str) {
        self.buf.push(b'"');
        for &b in s.as_bytes() {
            if matches!(b, b'\\' | b'"' | 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f | 0x7f) {
                self.buf.push(b'\\');
            }
            self.buf.push(b);
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
