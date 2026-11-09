//! The messages a leg's ladder is built from.

/// The Contact a message of `cseq`'s method states: the one a request that
/// opens a dialog, and its responses, name (RFC 3261 §8.1.1.8, §12.1.1).
fn contact(cseq: &str, uri: &str) -> String {
    let opens = ["INVITE", "SUBSCRIBE", "NOTIFY", "REFER"];
    match cseq.split_whitespace().last() {
        Some(method) if opens.contains(&method) => format!("Contact: <{uri}>\r\n"),
        _ => String::new(),
    }
}

pub(super) fn response(status: u16, cseq: &str) -> String {
    let contact = contact(cseq, "sip:bob@127.0.0.1:5080");
    format!(
        "SIP/2.0 {status} Whatever\r\n\
             {contact}Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-a\r\n\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>;tag=b1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
    )
}

pub(super) fn request(method: &str, cseq: &str) -> String {
    let contact = contact(cseq, "sip:bob@127.0.0.1:5080");
    format!(
        "{method} sip:alice@127.0.0.1:5060 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-x\r\n\
             Max-Forwards: 70\r\n\
             {contact}\
             From: <sip:bob@example.test>;tag=b1\r\n\
             To: <sip:alice@example.test>;tag=a1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
    )
}

/// The response this leg sends to a [`request`] the peer sent it: the
/// request's own From and To (RFC 3261 §8.2.6.2).
pub(super) fn reply(status: u16, cseq: &str) -> String {
    let contact = contact(cseq, "sip:alice@127.0.0.1:5060");
    format!(
        "SIP/2.0 {status} Whatever\r\n\
             {contact}Via: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-x\r\n\
             From: <sip:bob@example.test>;tag=b1\r\n\
             To: <sip:alice@example.test>;tag=a1\r\n\
             Call-ID: call-a\r\n\
             CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
    )
}

/// A request this leg TOOK, on the dialog `to_tag` names, with `extra`
/// headers ahead of the CSeq.
pub(super) fn taken(method: &str, cseq: &str, to_tag: Option<&str>, extra: &str) -> String {
    let tag = to_tag.map(|t| format!(";tag={t}")).unwrap_or_default();
    let contact = contact(cseq, "sip:alice@127.0.0.1:5060");
    let branch = cseq.replace(' ', "-");
    format!(
        "{method} sip:bob@127.0.0.1:5080 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-{branch}\r\n\
             Max-Forwards: 70\r\n\
             {contact}\
             From: <sip:alice@example.test>;tag=a1\r\n\
             To: <sip:bob@example.test>{tag}\r\n\
             Call-ID: call-a\r\n\
             {extra}CSeq: {cseq}\r\n\
             Content-Length: 0\r\n\r\n"
    )
}
