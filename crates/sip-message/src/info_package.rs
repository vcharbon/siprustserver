//! RFC 6086 Info Packages, read and stated by a UA that receives none on its
//! own behalf: whether an `INFO` names a package, and the `Recv-Info` set that
//! UA states.

use crate::header::HeaderName;
use crate::types::SipRequest;

/// `Info-Package` (RFC 6086 §7.2): the one package an `INFO` names.
pub const INFO_PACKAGE: &str = "Info-Package";
/// `Recv-Info` (RFC 6086 §7.3): the packages a UA is willing to receive.
pub const RECV_INFO: &str = "Recv-Info";

/// Whether `req` names an Info Package; a legacy `INFO` names none (RFC 6086
/// §4.2.1).
pub fn names_info_package(req: &SipRequest) -> bool {
    req.has(&HeaderName::from(INFO_PACKAGE))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::custom::CustomParser;
    use crate::SipParser;

    fn info(extra: &str) -> SipRequest {
        let raw = format!(
            "INFO sip:as@192.0.2.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 192.0.2.2:5070;branch=z9hG4bK-i1\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@192.0.2.2>;tag=a1\r\n\
To: <sip:as@192.0.2.1>;tag=b1\r\n\
Call-ID: info-package\r\n\
CSeq: 2 INFO\r\n\
{extra}Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            crate::SipMessage::Request(r) => r,
            _ => panic!("expected request"),
        }
    }

    #[test]
    fn a_packaged_info_names_its_package_in_any_casing() {
        assert!(names_info_package(&info("Info-Package: foo\r\n")));
        assert!(names_info_package(&info("info-package: foo\r\n")));
    }

    #[test]
    fn a_legacy_info_names_none() {
        assert!(!names_info_package(&info("")));
    }
}
