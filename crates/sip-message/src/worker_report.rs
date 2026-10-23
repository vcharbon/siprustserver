//! `X-Readiness` and `X-Incarnation`: what a worker states about itself on its
//! answer to a peer proxy's out-of-dialog OPTIONS (ADR-0050 §2).
//!
//! ```text
//! X-Readiness: new=accept
//! X-Readiness: new=refuse; cause=backend
//! X-Incarnation: 1792155573698
//! ```
//!
//! `new` says whether the worker takes a new call; `cause` is free text naming
//! why it refuses, for metrics and logs. `X-Incarnation` names the worker's
//! process by its boot generation, so a restart at the same address reads as a
//! new process. The first instance of each header wins.

use crate::header::HeaderName;
use crate::types::SipResponse;

/// The wire name of the new-call acceptance header.
pub const X_READINESS: &str = "X-Readiness";
/// The wire name of the process boot generation header.
pub const X_INCARNATION: &str = "X-Incarnation";

/// Whether a worker takes a new call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NewCalls {
    Accept,
    Refuse,
}

/// One `X-Readiness` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Readiness {
    pub new: NewCalls,
    /// Why new calls are refused; `None` when accepted or unstated.
    pub cause: Option<String>,
}

impl Readiness {
    /// `new=accept`.
    pub fn accept() -> Self {
        Self { new: NewCalls::Accept, cause: None }
    }

    /// `new=refuse; cause=<cause>`.
    pub fn refuse(cause: &str) -> Self {
        Self { new: NewCalls::Refuse, cause: Some(cause.to_string()) }
    }

    /// The header value, as written on the wire.
    pub fn header_value(&self) -> String {
        match (&self.new, &self.cause) {
            (NewCalls::Accept, _) => "new=accept".to_string(),
            (NewCalls::Refuse, Some(cause)) => format!("new=refuse; cause={cause}"),
            (NewCalls::Refuse, None) => "new=refuse".to_string(),
        }
    }

    /// Read one header value. `None` when `new` is absent or neither `accept`
    /// nor `refuse`; parameter names and the `new` value are case-insensitive.
    pub fn parse(value: &str) -> Option<Self> {
        let mut new = None;
        let mut cause = None;
        for param in value.split(';') {
            let Some((name, val)) = param.split_once('=') else { continue };
            let (name, val) = (name.trim(), val.trim());
            if name.eq_ignore_ascii_case("new") {
                new = if val.eq_ignore_ascii_case("accept") {
                    Some(NewCalls::Accept)
                } else if val.eq_ignore_ascii_case("refuse") {
                    Some(NewCalls::Refuse)
                } else {
                    None
                };
            } else if name.eq_ignore_ascii_case("cause") && !val.is_empty() {
                cause = Some(val.to_string());
            }
        }
        Some(Self { new: new?, cause })
    }
}

/// The `X-Readiness` `resp` carries. `None` when absent or unreadable.
pub fn readiness(resp: &SipResponse) -> Option<Readiness> {
    resp.raw(HeaderName::from(X_READINESS)).next().and_then(Readiness::parse)
}

/// The `X-Incarnation` `resp` carries. `None` when absent or not an unsigned
/// integer.
pub fn incarnation(resp: &SipResponse) -> Option<u64> {
    resp.raw(HeaderName::from(X_INCARNATION)).next().and_then(|v| v.trim().parse().ok())
}

/// The `X-Incarnation` header value for boot generation `gen`.
pub fn incarnation_value(gen: u64) -> String {
    gen.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::custom::CustomParser;
    use crate::parser::SipParser;
    use crate::types::SipMessage;

    fn response(headers: &str) -> SipResponse {
        let raw = format!(
            "SIP/2.0 200 OK\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-probe\r\n\
             From: <sip:probe@10.0.0.1>;tag=probe\r\n\
             To: <sip:probe@10.0.0.2>;tag=uas\r\n\
             Call-ID: probe@10.0.0.1\r\n\
             CSeq: 1 OPTIONS\r\n\
             {headers}\
             Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Response(r) => r,
            SipMessage::Request(_) => panic!("expected a response"),
        }
    }

    #[test]
    fn each_value_renders_and_reads_back() {
        for r in [Readiness::accept(), Readiness::refuse("backend"), Readiness::refuse("draining")]
        {
            let wire = format!("{X_READINESS}: {}\r\n", r.header_value());
            assert_eq!(readiness(&response(&wire)), Some(r));
        }
        assert_eq!(Readiness::refuse("cordoned").header_value(), "new=refuse; cause=cordoned");
        assert_eq!(Readiness::accept().header_value(), "new=accept");
    }

    #[test]
    fn names_and_the_new_value_are_case_insensitive_and_spacing_is_free() {
        let r = response("x-readiness:NEW = Refuse ;Cause=backend\r\n");
        assert_eq!(readiness(&r), Some(Readiness::refuse("backend")));
    }

    #[test]
    fn an_absent_or_unreadable_value_reads_none() {
        assert_eq!(readiness(&response("")), None);
        assert_eq!(readiness(&response("X-Readiness: new=maybe\r\n")), None);
        assert_eq!(readiness(&response("X-Readiness: cause=backend\r\n")), None);
    }

    #[test]
    fn the_first_instance_wins() {
        let r = response("X-Readiness: new=refuse; cause=a\r\nX-Readiness: new=accept\r\n");
        assert_eq!(readiness(&r), Some(Readiness::refuse("a")));
    }

    #[test]
    fn an_incarnation_reads_as_an_unsigned_integer() {
        let wire = format!("{X_INCARNATION}: {}\r\n", incarnation_value(1_792_155_573_698));
        assert_eq!(incarnation(&response(&wire)), Some(1_792_155_573_698));
        assert_eq!(incarnation(&response("X-Incarnation: -1\r\n")), None);
        assert_eq!(incarnation(&response("")), None);
    }
}
