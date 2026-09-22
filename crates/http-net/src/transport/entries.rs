//! Readings of the [`HTTP_TAG`](super::HTTP_TAG) channel: the exchanges a
//! ladder draws ([`to_http_entries`]) and the exchanges one client lane made
//! ([`CapturedExchange`], read by `RecordingHttpNetwork::exchanges`).
//!
//! The ladder's row source is the served side whenever a recorded service
//! handled the request; the client side is drawn only for an exchange no
//! service saw (a cut, a stall, a mid-flight error).

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

/// Every exchange `events` record, in request order.
pub fn to_http_entries(events: &[Stamped<HttpNetworkEvent>]) -> Vec<RecordedHttpEntry> {
    let mut clients: HashMap<u64, &LaneKey> = HashMap::new();
    let mut answered: HashSet<u64> = HashSet::new();
    let mut client_ends: HashMap<u64, &Stamped<HttpNetworkEvent>> = HashMap::new();
    let mut served_ends: HashMap<u64, &Stamped<HttpNetworkEvent>> = HashMap::new();
    for e in events {
        match &e.event {
            HttpNetworkEvent::Sent { client, .. } => {
                clients.insert(e.seq, client);
            }
            HttpNetworkEvent::Served { exchange: Some(x), .. } => {
                answered.insert(*x);
            }
            HttpNetworkEvent::Served { .. } => {}
            HttpNetworkEvent::Received { exchange, .. } => {
                client_ends.insert(*exchange, e);
            }
            HttpNetworkEvent::Answered { served, .. } => {
                served_ends.insert(*served, e);
            }
        }
    }
    let ending = |end: Option<&&Stamped<HttpNetworkEvent>>| match end {
        Some(e) => match &e.event {
            HttpNetworkEvent::Received { outcome, .. }
            | HttpNetworkEvent::Answered { outcome, .. } => {
                (Some(e.seq), Some(e.at_ms), Some(outcome.clone()))
            }
            _ => (None, None, None),
        },
        None => (None, None, None),
    };
    let mut entries = Vec::new();
    for e in events {
        match &e.event {
            HttpNetworkEvent::Served { service, peer, exchange, request } => {
                let (reply_seq, reply_at_ms, outcome) = ending(served_ends.get(&e.seq));
                entries.push(RecordedHttpEntry {
                    seq: e.seq,
                    at_ms: e.at_ms,
                    requester: exchange.and_then(|x| clients.get(&x)).map(|c| (*c).clone()),
                    peer: *peer,
                    service: service.clone(),
                    request: request.clone(),
                    reply_seq,
                    reply_at_ms,
                    outcome,
                    served: true,
                });
            }
            HttpNetworkEvent::Sent { client, dst, request } if !answered.contains(&e.seq) => {
                let (reply_seq, reply_at_ms, outcome) = ending(client_ends.get(&e.seq));
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
    events
        .iter()
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
