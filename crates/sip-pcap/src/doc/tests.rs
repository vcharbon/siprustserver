use super::*;

/// A request and a response each answer their own CSeq, From and To.
#[test]
fn a_summary_answers_its_cseq_and_parties_whatever_its_kind() {
    let party =
        |uri: &str, tag: Option<&str>| Party { uri: uri.to_string(), tag: tag.map(str::to_string) };
    let request = Summary::Request {
        method: "INVITE".to_string(),
        uri: "sip:bob@example.com".to_string(),
        cseq: CSeqJson { seq: 1, method: "INVITE".to_string() },
        from: party("sip:alice@example.com", Some("a1")),
        to: party("sip:bob@example.com", None),
    };
    let response = Summary::Response {
        status: 200,
        reason: "OK".to_string(),
        cseq: CSeqJson { seq: 2, method: "BYE".to_string() },
        from: party("sip:bob@example.com", Some("b1")),
        to: party("sip:alice@example.com", Some("a1")),
    };
    assert_eq!((request.cseq().seq, request.cseq().method.as_str()), (1, "INVITE"));
    assert_eq!(request.from().tag.as_deref(), Some("a1"));
    assert_eq!(request.to().tag, None);
    assert_eq!((response.cseq().seq, response.cseq().method.as_str()), (2, "BYE"));
    assert_eq!(response.from().uri, "sip:bob@example.com");
    assert_eq!(response.to().tag.as_deref(), Some("a1"));
}
