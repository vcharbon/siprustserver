//! The HTTP plane of a run's ladder: the exchanges recorded on the run's
//! recorder ([`http_net::HTTP_TAG`]) as rows of the report's doc.
//!
//! An exchange is a request row from the requester's lane to the service lane
//! and a reply row back, each at the `seq` of its own record, so the rows sit
//! between the SIP messages they happened between. The requester is the
//! recording client's lane when one sent the request; otherwise the peer
//! address the served side saw, mapped onto a registered lane when one matches.

use std::net::SocketAddr;

use http_net::{HttpOutcome, HttpRequest, HttpResponse, RecordedHttpEntry};
use layer_harness::{Lane as RecLane, NetworkTag};
use seq_report::{Lane, LaneKind, RowKind, SeqDoc, SeqRow};

/// The lane id of a requester seen only by its address.
const PEER_LANE_PREFIX: &str = "http-client ";

/// `doc` with the HTTP exchanges of `entries` drawn on it: the rows, and any
/// lane they need that the SIP projection did not declare.
pub fn with_rows(mut doc: SeqDoc, entries: &[RecordedHttpEntry], rec_lanes: &[RecLane]) -> SeqDoc {
    for e in entries {
        let service = service_lane(&mut doc, &e.service, rec_lanes);
        let requester = requester_lane(&mut doc, e, rec_lanes);
        doc.rows.push(SeqRow {
            at_ms: e.at_ms as i64,
            seq: e.seq,
            from: requester.clone(),
            to: Some(service.clone()),
            label: format!("{} {}", e.request.method, e.request.path),
            detail: Some(request_detail(&e.request)),
            conn: None,
            kind: RowKind::Http {
                delivered: e.served || matches!(e.outcome, Some(HttpOutcome::Response(_))),
            },
        });
        let (Some(outcome), Some(seq), Some(at_ms)) = (&e.outcome, e.reply_seq, e.reply_at_ms)
        else {
            continue;
        };
        let (label, detail, delivered) = match outcome {
            HttpOutcome::Response(resp) => (resp.status.to_string(), response_detail(resp), true),
            HttpOutcome::Abort => {
                ("reset".to_string(), "connection closed without a response".to_string(), false)
            }
            HttpOutcome::Abandoned => (
                "no reply".to_string(),
                "the caller stopped waiting before an answer".to_string(),
                false,
            ),
            HttpOutcome::Error(reason) => ("no reply".to_string(), reason.clone(), false),
        };
        doc.rows.push(SeqRow {
            at_ms: at_ms as i64,
            seq,
            from: service,
            to: Some(requester),
            label,
            detail: Some(detail),
            conn: None,
            kind: RowKind::Http { delivered },
        });
    }
    doc
}

/// The service lane `key`, declared (named as registered) on first use.
fn service_lane(doc: &mut SeqDoc, key: &str, rec_lanes: &[RecLane]) -> String {
    if !doc.lanes.iter().any(|l| l.id == key) {
        let label = rec_lanes
            .iter()
            .find(|l| l.key == key)
            .and_then(|l| l.names.first().map(|n| format!("{n} ({})", l.addr)))
            .unwrap_or_else(|| key.to_string());
        doc.lanes.push(Lane::new(key, label, LaneKind::Service));
    }
    key.to_string()
}

/// The lane an exchange's request leaves from.
fn requester_lane(doc: &mut SeqDoc, e: &RecordedHttpEntry, rec_lanes: &[RecLane]) -> String {
    if let Some(client) = &e.requester {
        return ensure_lane(doc, client, LaneKind::Sut);
    }
    let Some(peer) = e.peer else {
        return ensure_lane(doc, &format!("{PEER_LANE_PREFIX}?"), LaneKind::Sut);
    };
    if let Some(lane) = rec_lanes.iter().find(|l| l.addr == peer) {
        return ensure_lane(doc, &lane.key.clone(), LaneKind::Sut);
    }
    if let Some(lane) = only_core_lane_at(rec_lanes, peer) {
        return ensure_lane(doc, &lane.key.clone(), LaneKind::Sut);
    }
    let id = format!("{PEER_LANE_PREFIX}{}", peer.ip());
    ensure_lane(doc, &id, LaneKind::Sut)
}

/// The one core-fabric (SUT) lane on the peer's host, if exactly one.
fn only_core_lane_at(rec_lanes: &[RecLane], peer: SocketAddr) -> Option<&RecLane> {
    let mut at =
        rec_lanes.iter().filter(|l| l.network == NetworkTag::Core && l.addr.ip() == peer.ip());
    match (at.next(), at.next()) {
        (Some(lane), None) => Some(lane),
        _ => None,
    }
}

/// `id`, declared as a lane of `kind` when the doc does not have it yet.
fn ensure_lane(doc: &mut SeqDoc, id: &str, kind: LaneKind) -> String {
    if !doc.lanes.iter().any(|l| l.id == id) {
        let label = id
            .strip_prefix(PEER_LANE_PREFIX)
            .map_or_else(|| id.to_string(), |ip| format!("http client ({ip})"));
        doc.lanes.push(Lane::new(id, label, kind));
    }
    id.to_string()
}

fn request_detail(req: &HttpRequest) -> String {
    format!("{} {}\n{}", req.method, req.path, pretty(&req.headers, &req.body))
}

fn response_detail(resp: &HttpResponse) -> String {
    format!("{}\n{}", resp.status, pretty(&resp.headers, &resp.body))
}

/// Headers, a blank line, and the body pretty-printed when it is JSON.
fn pretty(headers: &[(String, String)], body: &[u8]) -> String {
    let mut out: String = headers.iter().map(|(k, v)| format!("{k}: {v}\n")).collect();
    out.push('\n');
    match serde_json::from_slice::<serde_json::Value>(body) {
        Ok(json) => out.push_str(&serde_json::to_string_pretty(&json).unwrap_or_default()),
        Err(_) => out.push_str(&String::from_utf8_lossy(body)),
    }
    out
}
