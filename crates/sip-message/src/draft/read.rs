//! The read surface of a [`Draft`]: its lines, and the values its headers
//! carry by cardinality, as a frozen message reads them.

use bytes::Bytes;

use crate::error::SipParseError;
use crate::header::{HeaderName, ListHeader, SingleHeader, TokenSet};
use crate::sip_str::SipStr;

use super::edit::Draft;
use super::entry::Entry;
use super::start::StartKind;

impl<S: StartKind> Draft<S> {
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn has(&self, name: &HeaderName) -> bool {
        let key = name.key();
        self.entries.iter().any(|e| e.is_key(&key))
    }

    /// Every line of `name` as text, in wire order.
    pub fn raw(&self, name: &HeaderName) -> Vec<SipStr> {
        let key = name.key();
        self.entries.iter().filter(|e| e.is_key(&key)).map(Entry::text).collect()
    }

    /// Every value of the list header `H` the draft carries.
    pub fn values<H: ListHeader>(&self) -> Result<Vec<H>, SipParseError> {
        let name = H::header_name();
        let key = name.key();
        let mut values = Vec::new();
        for entry in self.entries.iter().filter(|e| e.is_key(&key)) {
            values.extend(entry.read::<H>()?);
        }
        Ok(values)
    }

    /// The value of the single-valued `H`, or `None` when the draft carries
    /// none. A second line of it, which no freeze lets out, is an error.
    pub fn header<H: SingleHeader>(&self) -> Option<Result<H, SipParseError>> {
        let name = H::header_name();
        let key = name.key();
        let mut lines = self.entries.iter().filter(|e| e.is_key(&key));
        let entry = lines.next()?;
        if lines.next().is_some() {
            return Some(Err(repeated(&name)));
        }
        Some(entry.read_one::<H>())
    }

    /// The one token set every line of `H` makes, or `None` when the draft
    /// carries none.
    pub fn token_set<H: TokenSet>(&self) -> Option<Result<H, SipParseError>> {
        match self.values::<H>() {
            Err(e) => Some(Err(e)),
            Ok(values) => H::union(values).map(Ok),
        }
    }

    pub fn body_bytes(&self) -> &Bytes {
        &self.body
    }
}

/// The refusal of a second line of the single-valued header `name`.
pub(super) fn repeated(name: &HeaderName) -> SipParseError {
    SipParseError::new(format!("Multiple {name} headers (RFC 3261 §7.3.1 — one value)"))
}
