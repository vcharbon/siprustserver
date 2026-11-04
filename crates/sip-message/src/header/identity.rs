//! The scalar identity headers: [`CallId`], [`CSeq`] and [`RAck`].

use crate::char_class::is_token_char;
use crate::error::SipParseError;
use crate::method::Method;
use crate::sip_str::SipStr;

use super::name::HeaderName;
use super::scan::parse_u32;
use super::value::{Folding, HeaderValue};
use super::wire::Wire;

/// The largest sequence number: a CSeq-num is below 2^31 (RFC 3261 §8.1.1.5),
/// and so is the RSeq a RAck's response-num echoes (RFC 3262 §7.1).
const SEQUENCE_MAX: u32 = i32::MAX as u32;

/// The `N` LWS-separated fields of `value`, exactly `N` of them.
fn lws_fields<'a, const N: usize>(
    value: &'a str,
    header: &str,
) -> Result<[&'a str; N], SipParseError> {
    let refused =
        || SipParseError::new(format!("{header} is not {N} LWS-separated fields: {value:?}"));
    let mut fields = value.split([' ', '\t']).filter(|f| !f.is_empty());
    let mut out = [""; N];
    for slot in &mut out {
        *slot = fields.next().ok_or_else(refused)?;
    }
    match fields.next() {
        Some(_) => Err(refused()),
        None => Ok(out),
    }
}

/// `1*DIGIT`, at most [`SEQUENCE_MAX`]: every byte an ASCII digit, nothing
/// trimmed.
fn sequence_number(field: &str, what: &str) -> Result<u32, SipParseError> {
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
        return Err(SipParseError::new(format!("{what} is not 1*DIGIT: {field:?}")));
    }
    let n = parse_u32(field, what)?;
    if n > SEQUENCE_MAX {
        return Err(SipParseError::new(format!("{what} is not below 2^31: {field:?}")));
    }
    Ok(n)
}

/// `Method = token`.
fn method_token(field: &str, header: &str) -> Result<Method, SipParseError> {
    if !field.bytes().all(is_token_char) {
        return Err(SipParseError::new(format!("{header} method is not a token: {field:?}")));
    }
    Ok(Method::from_wire(field))
}

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

    /// The host a Call-ID written `localid@host` names (RFC 3261 §8.1.1.4):
    /// the text after its last `@`, or `None` where it has none.
    pub fn host(&self) -> Option<&str> {
        self.0.as_str().trim().rsplit_once('@').map(|(_, host)| host).filter(|h| !h.is_empty())
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

#[deny(clippy::arithmetic_side_effects)]
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

    /// The next request in this dialog (RFC 3261 §12.2.1.1); `None` where the
    /// sequence number is the largest one, which has no next.
    pub fn stepped(&self, method: Method) -> Option<Self> {
        Some(Self { seq: self.seq.checked_add(1)?, method })
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

    /// `CSeq = 1*DIGIT LWS Method` (RFC 3261 §25.1), the number below 2^31 —
    /// the grammar the parser's wire gate holds a CSeq to.
    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let [seq, method] = lws_fields(raw.as_str(), "CSeq")?;
        Ok(Self {
            seq: sequence_number(seq, "CSeq sequence")?,
            method: method_token(method, "CSeq")?,
        })
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

    /// `RAck = response-num LWS CSeq-num LWS Method` (RFC 3262 §7.2), both
    /// numbers below 2^31.
    fn parse(raw: &SipStr) -> Result<Self, SipParseError> {
        let [rseq, seq, method] = lws_fields(raw.as_str(), "RAck")?;
        Ok(Self {
            rseq: sequence_number(rseq, "RAck response-num")?,
            seq: sequence_number(seq, "RAck CSeq-num")?,
            method: method_token(method, "RAck")?,
        })
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
    fn a_cseq_keeps_its_method_as_written() {
        let c = CSeq::parse(&SipStr::owned("314159 iNvItE")).unwrap();
        assert_eq!(c.seq(), 314159);
        assert_eq!(c.method(), &Method::Other("iNvItE".to_string()));
        assert_eq!(c.to_wire(), "314159 iNvItE");
        let c = CSeq::parse(&SipStr::owned("314159 INVITE")).unwrap();
        assert_eq!(c.method(), &Method::Invite);
    }

    #[test]
    fn dialog_stepping_keeps_the_sequence_contract() {
        let c = CSeq::new(1, Method::Invite);
        assert_eq!(c.stepped(Method::Bye).map(|s| s.to_wire()).as_deref(), Some("2 BYE"));
        assert_eq!(c.as_method(Method::Ack).to_wire(), "1 ACK");
        assert_eq!(CSeq::new(u32::MAX, Method::Invite).stepped(Method::Bye), None);
    }

    #[test]
    fn a_rack_reads_all_three_fields() {
        let r = RAck::parse(&SipStr::owned("776656 1 INVITE")).unwrap();
        assert_eq!((r.rseq(), r.seq()), (776656, 1));
        assert_eq!(r.to_wire(), "776656 1 INVITE");
        assert!(RAck::parse(&SipStr::owned("776656 1")).is_err());
    }

    /// RFC 3262 §7.2: `response-num LWS CSeq-num LWS Method`, the method a
    /// token and nothing after it.
    #[test]
    fn a_rack_is_three_lws_separated_fields() {
        let rack = |s: &str| RAck::parse(&SipStr::owned(s));
        let r = rack("  42 \t 7  INVITE ").expect("LWS runs separate the fields");
        assert_eq!((r.rseq(), r.seq(), r.method()), (42, 7, &Method::Invite));
        for refused in [
            "776656 1INVITE",
            "776656 1 INVITE/extra",
            "776656 1 INVITE trailing",
            "776656 1 \"INVITE\"",
            "776656+1 1 INVITE",
            "776656 2147483648 INVITE",
            "776656\u{a0} 1 INVITE",
            "776656 1\x0b INVITE",
        ] {
            assert!(rack(refused).is_err(), "{refused:?} is no RAck");
        }
    }

    /// The typed CSeq reads by the grammar the parser's wire gate holds:
    /// `1*DIGIT LWS Method`, the number below 2^31, the method a token.
    #[test]
    fn a_cseq_is_a_number_and_a_method_token_only() {
        let cseq = |s: &str| CSeq::parse(&SipStr::owned(s));
        assert_eq!(cseq("2147483647\tBYE").expect("the largest").seq(), 2147483647);
        for refused in [
            "1 OPTIONS trailing",
            "1 INVITE/extra",
            "1 \"INVITE\"",
            "1INVITE",
            "2147483648 INVITE",
            "-1 INVITE",
            "1\x0b INVITE",
            "1\x0c INVITE",
            "1\u{a0} INVITE",
        ] {
            assert!(cseq(refused).is_err(), "{refused:?} is no CSeq");
        }
    }

    #[test]
    fn an_empty_call_id_is_rejected() {
        assert!(CallId::parse(&SipStr::owned("   ")).is_err());
        assert_eq!(CallId::parse(&SipStr::owned(" a84b4c ")).unwrap().as_str(), "a84b4c");
    }

    #[test]
    fn a_call_id_names_the_host_after_its_last_at() {
        assert_eq!(CallId::new("a84b4c76e66710@pc33.example.com").host(), Some("pc33.example.com"));
        assert_eq!(CallId::new("x@y@10.0.0.1").host(), Some("10.0.0.1"));
        assert_eq!(CallId::new("a84b4c76e66710").host(), None);
        assert_eq!(CallId::new("a84b@").host(), None);
    }
}
