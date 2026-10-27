//! Every request that reaches a living worker draws its final there: what a
//! lost call still owes (ADR-0055 §11). A transaction the dead primary took
//! is its own, its repeats included wherever they land; ACK takes no answer.

use std::collections::HashSet;
use std::net::SocketAddr;

use sip_message::Method;
use sip_net_testkit::CaptureIndex;

/// The one request that takes no answer.
const UNANSWERED: [Method; 1] = [Method::Ack];

/// Whether each request of `capture` delivered to a worker of `living` drew
/// a final from it on its transaction, unless `dead` took that transaction.
pub(super) fn living_workers_answered(
    capture: &CaptureIndex,
    living: &[SocketAddr],
    dead: SocketAddr,
) -> bool {
    let dead_took: HashSet<_> = capture
        .iter()
        .filter(|(_, e, _)| e.delivered && e.to == dead)
        .filter_map(|(_, _, m)| m.request_txn(&UNANSWERED))
        .collect();
    capture.iter().filter(|(_, e, _)| e.delivered && living.contains(&e.to)).all(|(_, e, m)| {
        let Some(key) = m.request_txn(&UNANSWERED) else { return true };
        let finals = capture.transaction(key).map(|t| t.finals.as_slice()).unwrap_or_default();
        dead_took.contains(key)
            || finals.iter().any(|f| capture.entry(*f).is_some_and(|f| f.from == e.to))
    })
}

#[cfg(test)]
mod tests {
    use sip_net_testkit::RecordedSipEntry;

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
        let entries = vec![
            entry("127.0.0.1:5080", WORKER, message("BYE sip:b@x SIP/2.0", "BYE")),
            entry(WORKER, "127.0.0.1:5080", message("SIP/2.0 481 Gone", "BYE")),
        ];
        assert!(living_workers_answered(
            &CaptureIndex::new(entries),
            &living(),
            DEAD.parse().unwrap()
        ));
    }

    #[test]
    fn a_request_left_without_its_final_fails() {
        let entries = vec![
            entry("127.0.0.1:5080", WORKER, message("INFO sip:b@x SIP/2.0", "INFO")),
            entry(WORKER, "127.0.0.1:5080", message("SIP/2.0 100 Trying", "INFO")),
        ];
        assert!(!living_workers_answered(
            &CaptureIndex::new(entries),
            &living(),
            DEAD.parse().unwrap()
        ));
    }

    #[test]
    fn a_transaction_the_dead_worker_took_owes_nothing_where_its_repeats_land() {
        let invite = || message("INVITE sip:b@x SIP/2.0", "INVITE");
        let entries = vec![
            entry("127.0.0.1:5080", DEAD, invite()),
            entry("127.0.0.1:5080", WORKER, invite()),
        ];
        assert!(living_workers_answered(
            &CaptureIndex::new(entries),
            &living(),
            DEAD.parse().unwrap()
        ));
    }
}
