//! A backlog of router work: out-of-dialog requests of a method a worker does
//! not serve, sent as raw datagrams in batches a millisecond apart, so no
//! instant brings a bind more than its inbound queue holds. Each is one event
//! the worker's router takes and refuses `405` (RFC 3261 §8.2.1), opening no
//! call.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use scenario_harness::Agent;
use sip_net::{PreIngressAction, PreIngressHook};

/// The method of a backlog request: outside every worker's `Allow`.
pub const BACKLOG_METHOD: &str = "MESSAGE";

/// Requests sent at one instant: half a SUT bind's inbound queue.
const BATCH: usize = 128;

/// Send `n` out-of-dialog [`BACKLOG_METHOD`] requests from `from` to `to`,
/// each its own transaction, over `n / BATCH` milliseconds; `label` keeps one
/// burst's Call-IDs apart from another's.
pub async fn send_backlog(from: &Agent, to: SocketAddr, n: usize, label: &str) {
    let via = from.addr();
    for i in 0..n {
        if i > 0 && i % BATCH == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let wire = format!(
            "{BACKLOG_METHOD} sip:{to} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {via};branch=z9hG4bK-{label}-{i}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:backlog@{via}>;tag={label}-{i}\r\n\
             To: <sip:worker@{to}>\r\n\
             Call-ID: {label}-{i}@{via}\r\n\
             CSeq: 1 {BACKLOG_METHOD}\r\n\
             Content-Length: 0\r\n\r\n"
        );
        from.try_send_datagram(wire.as_bytes(), to).await.expect("a backlog request leaves");
    }
}

/// The pre-ingress hook of a backlog sender's bind: it drops every datagram
/// the sender receives, the answers to its backlog, before its inbox, so a
/// burst of answers never overflows it. The worker's count of the requests it
/// refused (`unroutable_refused_of`) tells what was answered.
pub fn drop_backlog_answers() -> PreIngressHook {
    Arc::new(|_, _, _| PreIngressAction::Drop)
}
