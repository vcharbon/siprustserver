//! Every request that reaches a living worker draws its final there: what a
//! lost call still owes (ADR-0055 §11). A transaction the dead primary took
//! is its own, its repeats included wherever they land; ACK takes no answer.

use std::net::SocketAddr;

use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};
use sip_net_testkit::RecordedSipEntry;

/// A transaction: Call-ID, CSeq number and method, top Via branch.
type TxnKey = (String, u32, Method, String);

/// Whether each request of `entries` delivered to a worker of `living` drew
/// a final from it on its transaction, unless `dead` took that transaction.
pub(super) fn living_workers_answered(
    entries: &[RecordedSipEntry],
    living: &[SocketAddr],
    dead: SocketAddr,
) -> bool {
    let dead_took: Vec<TxnKey> = entries
        .iter()
        .filter(|e| e.delivered && e.to == dead)
        .filter_map(|e| request_key(&e.raw))
        .collect();
    let finals: Vec<(SocketAddr, TxnKey)> = entries
        .iter()
        .filter(|e| living.contains(&e.from))
        .filter_map(|e| Some((e.from, final_key(&e.raw)?)))
        .collect();
    entries.iter().filter(|e| e.delivered && living.contains(&e.to)).all(|e| {
        let Some(key) = request_key(&e.raw) else { return true };
        dead_took.contains(&key) || finals.iter().any(|(from, k)| *from == e.to && *k == key)
    })
}

/// The transaction of a request other than an ACK.
fn request_key(raw: &[u8]) -> Option<TxnKey> {
    let Ok(SipMessage::Request(req)) = CustomParser::new().parse(raw) else { return None };
    if *req.method() == Method::Ack {
        return None;
    }
    let branch = req.top_via().branch()?.to_string();
    Some((req.call_id().to_string(), req.cseq().seq(), req.method().clone(), branch))
}

/// The transaction a final answers.
fn final_key(raw: &[u8]) -> Option<TxnKey> {
    let Ok(SipMessage::Response(resp)) = CustomParser::new().parse(raw) else { return None };
    if resp.status() < 200 {
        return None;
    }
    let branch = resp.top_via().branch()?.to_string();
    Some((resp.call_id().to_string(), resp.cseq().seq(), resp.cseq().method().clone(), branch))
}

#[cfg(test)]
mod tests {
    use super::*;

    const WORKER: &str = "127.0.0.1:5092";
    const DEAD: &str = "127.0.0.1:5091";

    fn living() -> Vec<SocketAddr> {
        vec![WORKER.parse().unwrap()]
    }

    fn entry(from: &str, to: &str, raw: String) -> RecordedSipEntry {
        RecordedSipEntry {
            seq: 1,
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            raw: raw.into_bytes(),
            sent_ms: 1,
            received_ms: None,
            delivered: true,
            recv_note: None,
            reemit: None,
            from_lane: None,
            to_lane: None,
        }
    }

    fn message(start: &str, method: &str) -> String {
        format!(
            "{start}\r\nVia: SIP/2.0/UDP 127.0.0.1:5080;branch=z9hG4bK-1\r\n\
             From: <sip:a@x>;tag=a\r\nTo: <sip:b@x>;tag=b\r\nCall-ID: c1\r\n\
             CSeq: 2 {method}\r\nContent-Length: 0\r\n\r\n"
        )
    }

    #[test]
    fn a_request_a_living_worker_answered_passes() {
        let entries = [
            entry("127.0.0.1:5080", WORKER, message("BYE sip:b@x SIP/2.0", "BYE")),
            entry(WORKER, "127.0.0.1:5080", message("SIP/2.0 481 Gone", "BYE")),
        ];
        assert!(living_workers_answered(&entries, &living(), DEAD.parse().unwrap()));
    }

    #[test]
    fn a_request_left_without_its_final_fails() {
        let entries = [
            entry("127.0.0.1:5080", WORKER, message("INFO sip:b@x SIP/2.0", "INFO")),
            entry(WORKER, "127.0.0.1:5080", message("SIP/2.0 100 Trying", "INFO")),
        ];
        assert!(!living_workers_answered(&entries, &living(), DEAD.parse().unwrap()));
    }

    #[test]
    fn a_transaction_the_dead_worker_took_owes_nothing_where_its_repeats_land() {
        let invite = || message("INVITE sip:b@x SIP/2.0", "INVITE");
        let entries =
            [entry("127.0.0.1:5080", DEAD, invite()), entry("127.0.0.1:5080", WORKER, invite())];
        assert!(living_workers_answered(&entries, &living(), DEAD.parse().unwrap()));
    }
}
