//! [`SipStr`] — a string field that spans a shared, refcounted message image
//! instead of owning a copy of its bytes.
//!
//! A parsed message keeps ONE [`SharedText`] (the decoded datagram) and every
//! header name/value and structured sub-field is a byte range into it, so a
//! parse copies the packet once and never again. Cloning a `SipStr` — or a
//! whole `SipMessage` on a forwarding hop — bumps a refcount; it never copies
//! text.
//!
//! `Deref<Target = str>` makes a `SipStr` usable anywhere a `&str` is expected;
//! `Borrow<str>` + the `str`-delegating `Ord`/`Hash` make it a drop-in
//! `BTreeMap`/`HashMap` key that still looks up by `&str`.
//!
//! **Lifetime contract:** a `SipStr` pins the entire message image alive. It is
//! the right type for anything living inside a `SipMessage`, and the wrong type
//! for long-lived state (a call record, a transaction key) — copy out with
//! `.to_string()` at that boundary so a retained field cannot pin a datagram.

use std::borrow::{Borrow, Cow};
use std::cmp::Ordering;
use std::fmt;
use std::hash::{Hash, Hasher};
use std::ops::Deref;
use std::sync::Arc;

/// The decoded text image of one message: the single allocation every
/// [`SipStr`] of that message points into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedText(Arc<str>);

impl SharedText {
    /// Copy `text` into a fresh shared image — the one copy a parse makes.
    pub fn new(text: &str) -> Self {
        Self(Arc::from(text))
    }

    /// The whole image.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The `[start, start + len)` byte range of the image as a shared field.
    ///
    /// Both ends must be UTF-8 char boundaries within the image — every caller
    /// is a scanner that split on ASCII delimiters, so they always are; a range
    /// that is not (or that overflows `u32`) yields an owned copy rather than a
    /// panic, keeping a malformed span a memory cost instead of a crash.
    pub fn span(&self, start: usize, len: usize) -> SipStr {
        let end = start.saturating_add(len);
        let fits = u32::try_from(end).is_ok();
        match (fits, self.0.get(start..end)) {
            (true, Some(_)) => SipStr(Repr::Span {
                buf: Arc::clone(&self.0),
                start: start as u32,
                len: len as u32,
            }),
            (_, Some(s)) => SipStr::owned(s),
            (_, None) => SipStr::EMPTY,
        }
    }
}

impl From<String> for SharedText {
    fn from(s: String) -> Self {
        Self(Arc::from(s))
    }
}

/// A message string field: a span of the message's [`SharedText`], a `'static`
/// literal, or a standalone owned value (synthesized by a generator, or a
/// parsed value that had to be rewritten rather than sliced).
#[derive(Clone)]
pub struct SipStr(Repr);

#[derive(Clone)]
enum Repr {
    Static(&'static str),
    Span { buf: Arc<str>, start: u32, len: u32 },
}

impl SipStr {
    /// The empty string — no allocation.
    pub const EMPTY: SipStr = SipStr(Repr::Static(""));

    /// Wrap a literal. Free: no allocation, no refcount.
    pub const fn from_static(s: &'static str) -> Self {
        SipStr(Repr::Static(s))
    }

    /// Copy `s` into its own allocation — for values that do not exist in any
    /// message image (generated headers, rewritten values).
    pub fn owned(s: &str) -> Self {
        SipStr(Repr::Span { buf: Arc::from(s), start: 0, len: s.len() as u32 })
    }

    pub fn as_str(&self) -> &str {
        match &self.0 {
            Repr::Static(s) => s,
            Repr::Span { buf, start, len } => {
                let start = *start as usize;
                &buf[start..start + *len as usize]
            }
        }
    }

    /// A sub-range of THIS field, still sharing the same image — for a
    /// scanner that narrows an already-shared value (trim, prefix strip).
    /// `start`/`len` are relative to this field; an out-of-range or
    /// non-boundary range yields an owned copy.
    pub fn subspan(&self, start: usize, len: usize) -> SipStr {
        match &self.0 {
            Repr::Static(s) => match s.get(start..start.saturating_add(len)) {
                Some(sub) => SipStr(Repr::Static(sub)),
                None => SipStr::EMPTY,
            },
            Repr::Span { buf, start: base, len: base_len } => {
                let base = *base as usize;
                if start.saturating_add(len) > *base_len as usize {
                    return SipStr::EMPTY;
                }
                let abs = base + start;
                match buf.get(abs..abs + len) {
                    Some(_) => SipStr(Repr::Span {
                        buf: Arc::clone(buf),
                        start: abs as u32,
                        len: len as u32,
                    }),
                    None => SipStr::owned(&self.as_str()[start..start + len]),
                }
            }
        }
    }

    /// Re-express `sub` — a slice of THIS field's text, e.g. one entry of a
    /// comma-split value — as a span sharing the same buffer. Anything not
    /// actually inside this field is copied instead, so a caller mistake costs
    /// an allocation, never a panic or a wrong slice.
    pub fn reslice(&self, sub: &str) -> SipStr {
        let text = self.as_str();
        let origin = text.as_ptr() as usize;
        let start = sub.as_ptr() as usize;
        if start < origin || start + sub.len() > origin + text.len() {
            return SipStr::owned(sub);
        }
        self.subspan(start - origin, sub.len())
    }

    /// This field with surrounding ASCII/Unicode whitespace removed, still
    /// sharing the image (no copy).
    pub fn trimmed(&self) -> SipStr {
        let s = self.as_str();
        let trimmed = s.trim();
        if trimmed.len() == s.len() {
            return self.clone();
        }
        let offset = trimmed.as_ptr() as usize - s.as_ptr() as usize;
        self.subspan(offset, trimmed.len())
    }
}

impl Deref for SipStr {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<str> for SipStr {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<[u8]> for SipStr {
    fn as_ref(&self) -> &[u8] {
        self.as_str().as_bytes()
    }
}

impl Borrow<str> for SipStr {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

impl Default for SipStr {
    fn default() -> Self {
        SipStr::EMPTY
    }
}

/// Prints as the quoted string it stands for — a `SipStr` field must be
/// indistinguishable from the `String` it replaced in a `{:?}` dump.
impl fmt::Debug for SipStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl fmt::Display for SipStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl PartialEq for SipStr {
    fn eq(&self, other: &Self) -> bool {
        self.as_str() == other.as_str()
    }
}

impl Eq for SipStr {}

impl PartialOrd for SipStr {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SipStr {
    fn cmp(&self, other: &Self) -> Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl Hash for SipStr {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.as_str().hash(state)
    }
}

impl PartialEq<str> for SipStr {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for SipStr {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

impl PartialEq<String> for SipStr {
    fn eq(&self, other: &String) -> bool {
        self.as_str() == other.as_str()
    }
}

impl PartialEq<SipStr> for str {
    fn eq(&self, other: &SipStr) -> bool {
        self == other.as_str()
    }
}

impl PartialEq<SipStr> for &str {
    fn eq(&self, other: &SipStr) -> bool {
        *self == other.as_str()
    }
}

impl PartialEq<SipStr> for String {
    fn eq(&self, other: &SipStr) -> bool {
        self.as_str() == other.as_str()
    }
}

impl From<&str> for SipStr {
    fn from(s: &str) -> Self {
        SipStr::owned(s)
    }
}

impl From<String> for SipStr {
    fn from(s: String) -> Self {
        SipStr::owned(&s)
    }
}

impl From<Cow<'_, str>> for SipStr {
    fn from(s: Cow<'_, str>) -> Self {
        SipStr::owned(&s)
    }
}

impl From<SipStr> for String {
    fn from(s: SipStr) -> String {
        s.as_str().to_owned()
    }
}

/// Serializes as the plain string it stands for, so a `SipStr` field is wire-
/// indistinguishable from the `String` it replaced.
impl serde::Serialize for SipStr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for SipStr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        <String as serde::Deserialize>::deserialize(deserializer).map(|s| SipStr::owned(&s))
    }
}

impl From<&SipStr> for String {
    fn from(s: &SipStr) -> String {
        s.as_str().to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MSG: &str = "INVITE sip:bob@example.com SIP/2.0\r\nVia: SIP/2.0/UDP h;branch=z9\r\n";

    #[test]
    fn span_reads_the_image_range() {
        let text = SharedText::new(MSG);
        assert_eq!(text.span(0, 6).as_str(), "INVITE");
        assert_eq!(text.span(36, 3).as_str(), "Via");
    }

    #[test]
    fn span_shares_one_allocation() {
        let text = SharedText::new(MSG);
        let a = text.span(0, 6);
        let b = a.clone();
        assert_eq!(a.as_str().as_ptr(), b.as_str().as_ptr());
        assert_eq!(a.as_str().as_ptr(), text.as_str().as_ptr());
    }

    #[test]
    fn out_of_range_span_is_empty_not_a_panic() {
        let text = SharedText::new(MSG);
        assert_eq!(text.span(MSG.len(), 10).as_str(), "");
    }

    #[test]
    fn non_boundary_span_falls_back_to_owned() {
        // 'é' occupies bytes 0..2; a span ending mid-char must not panic.
        let text = SharedText::new("é-tag");
        assert_eq!(text.span(0, 1).as_str(), "");
    }

    #[test]
    fn trimmed_re_spans_the_image_instead_of_copying() {
        let text = SharedText::new("  padded  ");
        let t = text.span(0, 10).trimmed();
        assert_eq!(t.as_str(), "padded");
        // Same address as the image's byte 2 → a narrowed span, not a copy.
        assert_eq!(t.as_str().as_ptr(), text.as_str()[2..].as_ptr());
    }

    #[test]
    fn static_and_owned_compare_by_value() {
        assert_eq!(SipStr::from_static("tag"), SipStr::owned("tag"));
        assert_eq!(SipStr::from_static("tag"), "tag");
        assert_eq!(SipStr::from_static("tag").as_str(), "tag");
    }

    #[test]
    fn borrow_str_makes_it_a_map_key() {
        use std::collections::BTreeMap;
        let text = SharedText::new("branch=z9");
        let mut m: BTreeMap<SipStr, u8> = BTreeMap::new();
        m.insert(text.span(0, 6), 1);
        assert_eq!(m.get("branch"), Some(&1));
    }

    #[test]
    fn deref_gives_the_str_api() {
        let s = SipStr::from_static("SIP/2.0/UDP");
        assert!(s.starts_with("SIP/2.0"));
        assert_eq!(s.len(), 11);
        assert_eq!(s.split('/').count(), 3);
    }
}
