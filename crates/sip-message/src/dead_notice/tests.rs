use super::*;
use crate::parser::custom::CustomParser;
use crate::parser::SipParser;
use crate::types::SipMessage;

fn options(headers: &str) -> SipRequest {
    let raw = format!(
        "OPTIONS sip:10.0.0.2:5060 SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK-notice\r\n\
         Max-Forwards: 70\r\n\
         From: <sip:notice@10.0.0.1>;tag=n1\r\n\
         To: <sip:notice@10.0.0.2>\r\n\
         Call-ID: notice@10.0.0.1\r\n\
         CSeq: 1 OPTIONS\r\n\
         {headers}\
         Content-Length: 0\r\n\r\n"
    );
    match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    }
}

fn notice() -> DeadNotice {
    DeadNotice { ordinal: "b1".into(), at_ms: 1_792_155_573_698 }
}

#[test]
fn a_written_notice_reads_back_with_its_signature() {
    let value = notice().header_value(&[7u8; 16]).expect("fits");
    let read = DeadNotice::parse(&value).expect("readable");
    assert_eq!(read.notice, notice());
    assert_eq!(read.signature, Some(vec![7u8; 16]));
}

#[test]
fn a_notice_rides_an_options_request() {
    let value = notice().header_value(&[1u8; 16]).expect("fits");
    let req = options(&format!("{X_WORKER_DEAD}: {value}\r\n"));
    assert_eq!(dead_notice(&req).expect("read").notice, notice());
}

#[test]
fn only_the_first_notice_of_a_request_is_read() {
    let value = notice().header_value(&[1u8; 16]).expect("fits");
    let req = options(&format!("{X_WORKER_DEAD}: {value}\r\n{X_WORKER_DEAD}: b9; at=7\r\n"));
    assert_eq!(dead_notice(&req).expect("read").notice, notice());
    let req = options(&format!("{X_WORKER_DEAD}: ;\r\n{X_WORKER_DEAD}: {value}\r\n"));
    assert!(dead_notice(&req).is_none(), "an unreadable first notice is not passed over");
}

#[test]
fn a_notice_without_a_signature_reads_unsigned() {
    let read = DeadNotice::parse("b1; at=5").expect("readable");
    assert_eq!(read.notice, DeadNotice { ordinal: "b1".into(), at_ms: 5 });
    assert_eq!(read.signature, None);
    let bad = DeadNotice::parse("b1; at=5; sig=***").expect("readable");
    assert_eq!(bad.signature, None, "a sig that is not base64url is no signature");
}

#[test]
fn a_value_without_an_ordinal_or_an_instant_is_unreadable() {
    assert!(DeadNotice::parse("; at=5").is_none());
    assert!(DeadNotice::parse("b 1; at=5").is_none());
    assert!(DeadNotice::parse("b1; at=soon").is_none());
    assert!(DeadNotice::parse("b1").is_none());
    assert!(DeadNotice::parse(&"x".repeat(MAX_LEN + 1)).is_none());
}

#[test]
fn an_ordinal_that_is_no_token_writes_no_value() {
    let notice = DeadNotice { ordinal: "b;1".into(), at_ms: 5 };
    assert!(notice.header_value(&[1u8; 16]).is_none());
}

#[test]
fn the_signed_fields_name_the_statement_the_ordinal_and_the_instant() {
    let [domain, ordinal, at] = notice().signed_fields();
    assert_eq!(domain, b"x-worker-dead/1");
    assert_eq!(ordinal, b"b1");
    assert_eq!(at, b"1792155573698");
}
