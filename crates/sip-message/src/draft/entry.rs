//! [`Entry`] — one header line of a draft.
//!
//! An entry is either the bytes some message image already holds (a span, so
//! seeding a draft from a parsed message copies nothing) or a typed value that
//! renders itself when the draft freezes. Reading a thawed draft costs nothing
//! beyond the original parse: an entry becomes typed only when something edits
//! it.

use std::any::Any;

use crate::error::SipParseError;
use crate::header::{HeaderName, HeaderValue, Wire};
use crate::sip_str::SipStr;

/// The type-erased half of an entry: a value that can render and clone itself
/// without the draft knowing which header it is.
pub(crate) trait TypedEntry: std::fmt::Debug + Send + Sync + 'static {
    fn render_value(&self, out: &mut Wire);
    fn clone_entry(&self) -> Box<dyn TypedEntry>;
    fn as_any(&self) -> &dyn Any;
}

impl<H: HeaderValue> TypedEntry for H {
    fn render_value(&self, out: &mut Wire) {
        self.render(out)
    }

    fn clone_entry(&self) -> Box<dyn TypedEntry> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Debug)]
enum Value {
    /// Bytes exactly as some image holds them — never re-parsed, memcpy'd once
    /// at freeze.
    Raw(SipStr),
    Typed(Box<dyn TypedEntry>),
}

/// One header line: a name and what it carries.
#[derive(Debug)]
pub struct Entry {
    name: HeaderName,
    value: Value,
}

impl Entry {
    /// An entry holding wire bytes. The value is NOT parsed or validated.
    pub fn raw(name: HeaderName, value: impl Into<SipStr>) -> Self {
        Self { name, value: Value::Raw(value.into()) }
    }

    /// An entry holding a typed value, named by that value's header.
    pub fn typed<H: HeaderValue>(value: H) -> Self {
        Self { name: H::header_name(), value: Value::Typed(Box::new(value)) }
    }

    pub fn name(&self) -> &HeaderName {
        &self.name
    }

    /// Whether this line carries `name`. An entry that kept a caller's own
    /// spelling still answers to the header it names.
    pub fn is(&self, name: &HeaderName) -> bool {
        self.name.same_header(name)
    }

    /// Whether this entry still holds untouched wire bytes.
    pub fn is_raw(&self) -> bool {
        matches!(self.value, Value::Raw(_))
    }

    pub(crate) fn render_value(&self, out: &mut Wire) {
        match &self.value {
            Value::Raw(text) => out.str(text.as_str()),
            Value::Typed(value) => value.render_value(out),
        }
    }

    /// The entry's value as text. A raw entry hands back its span; a typed one
    /// renders into a fresh buffer.
    pub fn text(&self) -> SipStr {
        match &self.value {
            Value::Raw(text) => text.clone(),
            Value::Typed(value) => {
                let mut w = Wire::new();
                value.render_value(&mut w);
                SipStr::owned(w.as_str())
            }
        }
    }

    /// Every value this line carries, read as `H`. A typed entry already
    /// holding an `H` is handed back without a parse.
    pub(crate) fn read<H: HeaderValue>(&self) -> Result<Vec<H>, SipParseError> {
        if let Value::Typed(value) = &self.value {
            if let Some(typed) = value.as_any().downcast_ref::<H>() {
                return Ok(vec![typed.clone()]);
            }
        }
        H::parse_line(&self.text())
    }
}

impl Clone for Entry {
    fn clone(&self) -> Self {
        let value = match &self.value {
            Value::Raw(text) => Value::Raw(text.clone()),
            Value::Typed(value) => Value::Typed(value.clone_entry()),
        };
        Self { name: self.name.clone(), value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header::{self, Via};

    #[test]
    fn a_raw_entry_hands_back_its_bytes_untouched() {
        let e = Entry::raw(HeaderName::Via, SipStr::owned("SIP/2.0/UDP  h ;branch=z1"));
        assert!(e.is_raw());
        assert_eq!(e.text().as_str(), "SIP/2.0/UDP  h ;branch=z1");
    }

    #[test]
    fn a_typed_entry_is_read_back_without_a_reparse() {
        let via = Via::udp("h", 5060).with_branch("z9hG4bK1");
        let e = Entry::typed(via.clone());
        assert!(!e.is_raw());
        assert_eq!(e.read::<Via>().unwrap(), vec![via]);
    }

    #[test]
    fn a_raw_entry_parses_on_read() {
        let e = Entry::raw(HeaderName::From, SipStr::owned("<sip:a@h>;tag=1"));
        let values = e.read::<header::From>().unwrap();
        assert_eq!(values[0].tag(), Some("1"));
    }
}
