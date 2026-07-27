//! The scalar identity headers: [`CallId`], [`CSeq`] and [`RAck`].

use crate::error::SipParseError;
use crate::method::Method;
use crate::sip_str::SipStr;

use super::name::HeaderName;
use super::scan::{digits_end, parse_u32, skip_ws, sub, sub_trimmed};
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// The dialog's Call-ID — an opaque token compared byte-exactly.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallId(SipStr);

impl CallId {
    pub fn new(id: impl Into<SipStr>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

/// A Call-ID is an opaque token compared byte-exactly, so comparing one to the
/// text a caller holds is the comparison itself — not a conversion.
impl PartialEq<str> for CallId {
    fn eq(&self, other: &str) -> bool {
        self.0.as_str() == other
    }
}

impl PartialEq<&str> for CallId {
    fn eq(&self, other: &&str) -> bool {
        self.0.as_str() == *other
    }
}

impl PartialEq<String> for CallId {
    fn eq(&self, other: &String) -> bool {
        self.0.as_str() == other.as_str()
    }
}

/// The read surface hands out `&CallId`; a comparison against an owned one must
/// not have to dereference.
impl PartialEq<CallId> for &CallId {
    fn eq(&self, other: &CallId) -> bool {
        *self == other
    }
}

impl std::fmt::Display for CallId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0.as_str())
    }
}

impl HeaderValue for CallId {
    fn header_name() -> HeaderName {
        HeaderName::CallId
    }

    fn folding() -> Folding {
        Folding::Single
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let id = raw.trimmed();
        if id.is_empty() {
            return Err(SipParseError::new("Call-ID is empty"));
        }
        Ok(Self(id))
    }

    fn render(&self, out: &mut Wire) {
        out.str(self.0.as_str());
    }
}

/// The transaction sequence: `1*DIGIT Method`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSeq {
    seq: u32,
    method: Method,
}

impl CSeq {
    pub fn new(seq: u32, method: Method) -> Self {
        Self { seq, method }
    }

    pub fn seq(&self) -> u32 {
        self.seq
    }

    pub fn method(&self) -> &Method {
        &self.method
    }

    /// The next request in this dialog (RFC 3261 §12.2.1.1).
    pub fn stepped(&self, method: Method) -> Self {
        Self { seq: self.seq.saturating_add(1), method }
    }

    /// The same sequence under another method — what ACK and CANCEL carry.
    pub fn as_method(&self, method: Method) -> Self {
        Self { seq: self.seq, method }
    }
}

impl HeaderValue for CSeq {
    fn header_name() -> HeaderName {
        HeaderName::CSeq
    }

    fn folding() -> Folding {
        Folding::Single
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let start = skip_ws(bytes, 0);
        let end = digits_end(bytes, start);
        let seq = parse_u32(&value.as_str()[start..end], "CSeq sequence")?;
        let method = sub_trimmed(&value, end, bytes.len());
        if method.is_empty() {
            return Err(SipParseError::new(format!("CSeq has no method: {:?}", value.as_str())));
        }
        Ok(Self { seq, method: Method::from_wire(method.as_str()) })
    }

    fn render(&self, out: &mut Wire) {
        out.num(self.seq as u64);
        out.byte(b' ');
        out.str(self.method.as_str());
    }
}

/// RFC 3262 §7.2 RAck: `response-num CSeq-num Method`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RAck {
    rseq: u32,
    seq: u32,
    method: Method,
}

impl RAck {
    pub fn new(rseq: u32, seq: u32, method: Method) -> Self {
        Self { rseq, seq, method }
    }

    pub fn rseq(&self) -> u32 {
        self.rseq
    }

    pub fn seq(&self) -> u32 {
        self.seq
    }

    pub fn method(&self) -> &Method {
        &self.method
    }
}

impl HeaderValue for RAck {
    fn header_name() -> HeaderName {
        HeaderName::RAck
    }

    fn folding() -> Folding {
        Folding::Single
    }

    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let value = raw.trimmed();
        let bytes = value.as_bytes();
        let mut i = skip_ws(bytes, 0);
        let rseq_end = digits_end(bytes, i);
        let rseq = parse_u32(&value.as_str()[i..rseq_end], "RAck response-num")?;

        i = skip_ws(bytes, rseq_end);
        let seq_end = digits_end(bytes, i);
        let seq = parse_u32(&value.as_str()[i..seq_end], "RAck CSeq-num")?;

        let method = sub(&value, skip_ws(bytes, seq_end), bytes.len());
        if method.is_empty() || method.bytes().any(|b| b == b' ' || b == b'\t') {
            return Err(SipParseError::new(format!("RAck has no method: {:?}", value.as_str())));
        }
        Ok(Self { rseq, seq, method: Method::from_wire(method.as_str()) })
    }

    fn render(&self, out: &mut Wire) {
        out.num(self.rseq as u64);
        out.byte(b' ');
        out.num(self.seq as u64);
        out.byte(b' ');
        out.str(self.method.as_str());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cseq_folds_its_method_to_the_canonical_spelling() {
        let c = CSeq::parse(&SipStr::owned("314159 iNvItE")).unwrap();
        assert_eq!(c.seq(), 314159);
        assert_eq!(c.method(), &Method::Invite);
        assert_eq!(c.to_wire(), "314159 INVITE");
    }

    #[test]
    fn dialog_stepping_keeps_the_sequence_contract() {
        let c = CSeq::new(1, Method::Invite);
        assert_eq!(c.stepped(Method::Bye).to_wire(), "2 BYE");
        assert_eq!(c.as_method(Method::Ack).to_wire(), "1 ACK");
    }

    #[test]
    fn a_rack_reads_all_three_fields() {
        let r = RAck::parse(&SipStr::owned("776656 1 INVITE")).unwrap();
        assert_eq!((r.rseq(), r.seq()), (776656, 1));
        assert_eq!(r.to_wire(), "776656 1 INVITE");
        assert!(RAck::parse(&SipStr::owned("776656 1")).is_err());
    }

    #[test]
    fn an_empty_call_id_is_rejected() {
        assert!(CallId::parse(&SipStr::owned("   ")).is_err());
        assert_eq!(CallId::parse(&SipStr::owned(" a84b4c ")).unwrap().as_str(), "a84b4c");
    }
}
