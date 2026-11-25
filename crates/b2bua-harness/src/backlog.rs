//! Bursts of router work, sent as raw datagrams in batches a millisecond
//! apart, so no instant brings a bind more than its inbound queue holds:
//!
//! - a backlog: out-of-dialog requests of a method a worker does not serve,
//!   each one event the worker's router takes and refuses `405` (RFC 3261
//!   §8.2.1), opening no call;
//! - strays: BYEs naming dialogs that do not exist, each on a Call-ID of its
//!   own, that the worker answers `481` (§12.2.2).

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
    send_batched(from, to, n, |i| {
        format!(
            "{BACKLOG_METHOD} sip:{to} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {via};branch=z9hG4bK-{label}-{i}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:backlog@{via}>;tag={label}-{i}\r\n\
             To: <sip:worker@{to}>\r\n\
             Call-ID: {label}-{i}@{via}\r\n\
             CSeq: 1 {BACKLOG_METHOD}\r\n\
             Content-Length: 0\r\n\r\n"
        )
    })
    .await;
}

/// Send `n` stray BYEs from `from` to `to` over `n / BATCH` milliseconds:
/// each tagged as a request inside a dialog (RFC 3261 §12.2.1.1) that the
/// worker never had, on a Call-ID of its own, to the worker's bare address,
/// so its Request-URI names no call. `label` keeps one burst's Call-IDs apart
/// from another's.
pub async fn send_strays(from: &Agent, to: SocketAddr, n: usize, label: &str) {
    let via = from.addr();
    send_batched(from, to, n, |i| {
        format!(
            "BYE sip:worker@{to} SIP/2.0\r\n\
             Via: SIP/2.0/UDP {via};branch=z9hG4bK-{label}-{i}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:stray@{via}>;tag={label}-f{i}\r\n\
             To: <sip:worker@{to}>;tag={label}-t{i}\r\n\
             Call-ID: {label}-{i}@{via}\r\n\
             CSeq: 2 BYE\r\n\
             Content-Length: 0\r\n\r\n"
        )
    })
    .await;
}

/// Send the `n` datagrams `wire` writes from `from` to `to`, [`BATCH`] at a
/// time, a millisecond apart.
async fn send_batched(from: &Agent, to: SocketAddr, n: usize, wire: impl Fn(usize) -> String) {
    for i in 0..n {
        if i > 0 && i % BATCH == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        from.try_send_datagram(wire(i).as_bytes(), to).await.expect("a burst request leaves");
    }
}

/// The pre-ingress hook of a burst sender's bind: it drops every datagram
/// the sender receives, the answers to its burst, before its inbox, so a
/// burst of answers never overflows it. The worker's count of the requests it
/// refused (`unroutable_refused_of`) tells what was answered.
pub fn drop_backlog_answers() -> PreIngressHook {
    Arc::new(|_, _, _| PreIngressAction::Drop)
}
