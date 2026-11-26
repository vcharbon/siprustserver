//! Bursts of router work, sent as raw datagrams in batches a millisecond
//! apart, so no instant brings a bind more than its inbound queue holds:
//!
//! - a backlog: out-of-dialog requests of a method a worker does not serve,
//!   each naming a call by the `callRef` of its Request-URI, so it waits in
//!   the event queue among the work of calls: one event the worker's router
//!   takes, finds no call for and refuses `405` (RFC 3261 §8.2.1);
//! - strays: BYEs naming dialogs that do not exist, each on a Call-ID of its
//!   own, that the worker answers `481` (§12.2.2).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use scenario_harness::Agent;
use sip_net::{PreIngressAction, PreIngressHook};

/// The method of a backlog request: outside every worker's `Allow`.
pub const BACKLOG_METHOD: &str = "MESSAGE";

/// Requests sent at one instant: half a SUT bind's inbound queue.
const BATCH: usize = 128;

/// Send `n` out-of-dialog [`BACKLOG_METHOD`] requests from `from` to `to`,
/// each its own transaction naming a call `to` never had, over `n / BATCH`
/// milliseconds; `label` keeps one burst's Call-IDs apart from another's.
pub async fn send_backlog(from: &Agent, to: SocketAddr, n: usize, label: &str) {
    let via = from.addr();
    send_batched(from, to, n, |i| {
        format!(
            "{BACKLOG_METHOD} sip:{to};callRef=backlog-{label}-{i} SIP/2.0\r\n\
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
/// burst of answers never overflows it.
pub fn drop_backlog_answers() -> PreIngressHook {
    Arc::new(|_, _, _| PreIngressAction::Drop)
}

/// The answers a burst sender received, by status: its bind's pre-ingress
/// hook ([`hook`](Self::hook)) counts each response and drops every datagram
/// before the inbox, as [`drop_backlog_answers`] does.
#[derive(Clone, Default)]
pub struct BurstAnswers(Arc<Mutex<HashMap<u16, u64>>>);

impl BurstAnswers {
    /// The pre-ingress hook that counts into these answers.
    pub fn hook(&self) -> PreIngressHook {
        let counts = self.0.clone();
        Arc::new(move |arrival, _, _| {
            if let Some(status) = status_of(arrival.raw()) {
                let mut counts = counts.lock().unwrap_or_else(PoisonError::into_inner);
                *counts.entry(status).or_default() += 1;
            }
            PreIngressAction::Drop
        })
    }

    /// How many answers of `status` arrived.
    pub fn of(&self, status: u16) -> u64 {
        let counts = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        counts.get(&status).copied().unwrap_or(0)
    }
}

/// The status code of the response `datagram` holds; `None` for a request.
fn status_of(datagram: &[u8]) -> Option<u16> {
    let code = datagram.strip_prefix(b"SIP/2.0 ")?.get(..3)?;
    std::str::from_utf8(code).ok()?.parse().ok()
}
