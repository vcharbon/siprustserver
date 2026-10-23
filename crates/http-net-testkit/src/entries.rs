//! Readings of the [`HTTP_TAG`](super::HTTP_TAG) channel: the exchanges a
//! ladder draws ([`to_http_entries`]) and the exchanges one client lane made
//! ([`CapturedExchange`], read by `RecordingHttpNetwork::exchanges`).
//!
//! The ladder's row source is the served side whenever a recorded service
//! handled the request; the client side is drawn only for an exchange no
//! service saw (a cut, a stall, a mid-flight error). Both readings are in
//! `seq` order.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;

use layer_harness::{lane_key, LaneKey, Stamped};

use super::recording::{HttpNetworkEvent, HttpOutcome};
use super::HttpRequest;

/// What an exchange says of itself when the caller stopped waiting for it.
const ABANDONED: &str = "timed out: the caller dropped the request before an answer";

/// One exchange as the ladder draws it: the request, and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecordedHttpEntry {
    /// The global `seq` of the request's record.
    pub seq: u64,
    /// When the request was recorded (ms, the recorder's clock).
    pub at_ms: u64,
    /// The lane of the recording client that sent it, when one did.
    pub requester: Option<LaneKey>,
    /// The connection's remote address, when the served side had one.
    pub peer: Option<SocketAddr>,
    /// The lane of the address the request went to.
    pub service: LaneKey,
    /// The request as sent.
    pub request: HttpRequest,
    /// The `seq` of the ending record; `None` while the exchange is open.
    pub reply_seq: Option<u64>,
    /// When the ending record was recorded.
    pub reply_at_ms: Option<u64>,
    /// How it ended; `None` while open.
    pub outcome: Option<HttpOutcome>,
    /// `true` when the served side is the source (a recorded service saw it).
    pub served: bool,
}

/// Every exchange `events` record, in `seq` order of the requests.
///
/// A served request pairs with the client exchange it answers: by the
/// exchange the served record names (the simulated fabric runs the handler
/// inside the client's future), else, for a request that arrived on a socket,
/// with the earliest unpaired client request to the same address carrying
/// the same method, target and body that was sent before it and had not ended
/// when it arrived. A paired exchange is drawn once, from the served side,
/// with the client's lane; its end is the service's, unless the service
/// answered and the client never got the answer (a caller that stopped
/// waiting shows as such). An unpaired served request's end is the service's
/// view only.
pub fn to_http_entries(events: &[Stamped<HttpNetworkEvent>]) -> Vec<RecordedHttpEntry> {
    let mut events: Vec<&Stamped<HttpNetworkEvent>> = events.iter().collect();
    events.sort_by_key(|e| e.seq);
    let mut client_ends: HashMap<u64, &Stamped<HttpNetworkEvent>> = HashMap::new();
    let mut served_ends: HashMap<u64, &Stamped<HttpNetworkEvent>> = HashMap::new();
    for e in &events {
        match &e.event {
            HttpNetworkEvent::Received { exchange, .. } => {
                client_ends.insert(*exchange, e);
            }
            HttpNetworkEvent::Answered { served, .. } => {
                served_ends.insert(*served, e);
            }
            _ => {}
        }
    }
    let pairs = pair(&events, &client_ends);
    let paired: HashSet<u64> = pairs.values().copied().collect();
    let sent: HashMap<u64, &LaneKey> = events
        .iter()
        .filter_map(|e| match &e.event {
            HttpNetworkEvent::Sent { client, .. } => Some((e.seq, client)),
            _ => None,
        })
        .collect();
    let ending = |end: Option<&Stamped<HttpNetworkEvent>>| match end.map(|e| (e, &e.event)) {
        Some((e, HttpNetworkEvent::Received { outcome, .. }))
        | Some((e, HttpNetworkEvent::Answered { outcome, .. })) => {
            (Some(e.seq), Some(e.at_ms), Some(outcome.clone()))
        }
        _ => (None, None, None),
    };
    let mut entries = Vec::new();
    for e in &events {
        match &e.event {
            HttpNetworkEvent::Served { service, peer, request, .. } => {
                let client = pairs.get(&e.seq).copied();
                let end = drawn_end(
                    served_ends.get(&e.seq).copied(),
                    client.and_then(|c| client_ends.get(&c)).copied(),
                );
                let (reply_seq, reply_at_ms, outcome) = ending(end);
                entries.push(RecordedHttpEntry {
                    seq: e.seq,
                    at_ms: e.at_ms,
                    requester: client.and_then(|c| sent.get(&c)).map(|c| (*c).clone()),
                    peer: *peer,
                    service: service.clone(),
                    request: request.clone(),
                    reply_seq,
                    reply_at_ms,
                    outcome,
                    served: true,
                });
            }
            HttpNetworkEvent::Sent { client, dst, request } if !paired.contains(&e.seq) => {
                let (reply_seq, reply_at_ms, outcome) = ending(client_ends.get(&e.seq).copied());
                entries.push(RecordedHttpEntry {
                    seq: e.seq,
                    at_ms: e.at_ms,
                    requester: Some(client.clone()),
                    peer: None,
                    service: lane_key(*dst),
                    request: request.clone(),
                    reply_seq,
                    reply_at_ms,
                    outcome,
                    served: false,
                });
            }
            _ => {}
        }
    }
    entries
}

/// The end a paired exchange's reply row draws: the service's, unless the
/// service answered and the client never got that answer (it stopped waiting,
/// or the connection failed on the way back) — then the client's.
fn drawn_end<'a>(
    served: Option<&'a Stamped<HttpNetworkEvent>>,
    client: Option<&'a Stamped<HttpNetworkEvent>>,
) -> Option<&'a Stamped<HttpNetworkEvent>> {
    let outcome = |end: Option<&Stamped<HttpNetworkEvent>>| match end.map(|e| &e.event) {
        Some(HttpNetworkEvent::Received { outcome, .. })
        | Some(HttpNetworkEvent::Answered { outcome, .. }) => Some(outcome.clone()),
        _ => None,
    };
    match (outcome(served), outcome(client)) {
        (Some(HttpOutcome::Response(_)), Some(c)) if !matches!(c, HttpOutcome::Response(_)) => {
            client
        }
        (None, Some(_)) => client,
        _ => served.or(client),
    }
}

/// Served seq → the client exchange (its `Sent` seq) it answers.
fn pair(
    events: &[&Stamped<HttpNetworkEvent>],
    client_ends: &HashMap<u64, &Stamped<HttpNetworkEvent>>,
) -> HashMap<u64, u64> {
    let mut pairs = HashMap::new();
    let mut taken: HashSet<u64> = events
        .iter()
        .filter_map(|e| match &e.event {
            HttpNetworkEvent::Served { exchange: Some(x), .. } => Some(*x),
            _ => None,
        })
        .collect();
    for e in events {
        let HttpNetworkEvent::Served { service, exchange, request, .. } = &e.event else {
            continue;
        };
        if let Some(x) = exchange {
            pairs.insert(e.seq, *x);
            continue;
        }
        let client = events.iter().find_map(|c| match &c.event {
            HttpNetworkEvent::Sent { dst, request: sent, .. }
                if c.seq < e.seq
                    && !taken.contains(&c.seq)
                    && lane_key(*dst) == *service
                    && sent.method == request.method
                    && sent.path == request.path
                    && sent.body == request.body
                    && client_ends.get(&c.seq).is_none_or(|end| end.seq > e.seq) =>
            {
                Some(c.seq)
            }
            _ => None,
        });
        if let Some(c) = client {
            taken.insert(c);
            pairs.insert(e.seq, c);
        }
    }
    pairs
}

/// The outcome of one client exchange.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExchangeOutcome {
    /// A response came back.
    Response {
        /// HTTP status.
        status: u16,
        /// Response headers as `(name, value)` pairs.
        headers: Vec<(String, String)>,
        /// Response body bytes.
        body: Vec<u8>,
    },
    /// No response: the transport failed, or the caller stopped waiting.
    Error(String),
}

/// One request a client lane sent, with its destination, stamp and outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedExchange {
    /// When the request WENT OUT (ms, the recorder's clock).
    pub at_ms: i64,
    /// The destination the request went to.
    pub dst: SocketAddr,
    /// Request method (e.g. `"POST"`).
    pub method: String,
    /// Request path-and-query (e.g. `"/v1/admit"` or `"/routes?debug=true"`).
    pub path: String,
    /// Request headers as `(name, value)` pairs.
    pub req_headers: Vec<(String, String)>,
    /// Request body bytes.
    pub req_body: Vec<u8>,
    /// What came back. An exchange still open reads as abandoned.
    pub outcome: ExchangeOutcome,
}

/// The exchanges `client` sent, in request order.
pub(super) fn client_exchanges(
    events: &[Stamped<HttpNetworkEvent>],
    client: &LaneKey,
) -> Vec<CapturedExchange> {
    let ends: HashMap<u64, &HttpOutcome> = events
        .iter()
        .filter_map(|e| match &e.event {
            HttpNetworkEvent::Received { exchange, outcome } => Some((*exchange, outcome)),
            _ => None,
        })
        .collect();
    let mut sent: Vec<&Stamped<HttpNetworkEvent>> = events.iter().collect();
    sent.sort_by_key(|e| e.seq);
    sent.into_iter()
        .filter_map(|e| match &e.event {
            HttpNetworkEvent::Sent { client: from, dst, request } if from == client => {
                Some(CapturedExchange {
                    at_ms: e.at_ms as i64,
                    dst: *dst,
                    method: request.method.clone(),
                    path: request.path.clone(),
                    req_headers: request.headers.clone(),
                    req_body: request.body.clone(),
                    outcome: match ends.get(&e.seq) {
                        Some(HttpOutcome::Response(resp)) => ExchangeOutcome::Response {
                            status: resp.status,
                            headers: resp.headers.clone(),
                            body: resp.body.clone(),
                        },
                        Some(HttpOutcome::Error(reason)) => ExchangeOutcome::Error(reason.clone()),
                        Some(HttpOutcome::Abort) => {
                            ExchangeOutcome::Error("connection reset".to_string())
                        }
                        Some(HttpOutcome::Abandoned) | None => {
                            ExchangeOutcome::Error(ABANDONED.to_string())
                        }
                    },
                })
            }
            _ => None,
        })
        .collect()
}
