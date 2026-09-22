//! The HTTP plane of a run's report: the exchanges recorded on the run's
//! recorder ([`http_net::HTTP_TAG`]) as ladder rows, the per-service wire views
//! of the text report, and a scripted service's findings as report anomalies.
//!
//! An exchange is a request row from the requester's lane to the service lane
//! and a reply row back, each at the `seq` of its own record, so the rows sit
//! between the SIP messages they happened between. The requester is the
//! recording client's lane when one sent the request; otherwise the peer
//! address the served side saw, mapped onto a registered lane when one matches.

use std::collections::HashSet;
use std::net::SocketAddr;

use http_net::scripted::{HttpFinding, HttpFindingKind};
use http_net::{HttpOutcome, HttpRequest, HttpResponse, RecordedHttpEntry};
use layer_harness::{Lane as RecLane, NetworkTag};
use seq_report::{Anomaly, Lane, LaneKind, RowKind, SeqDoc, SeqRow};

use super::wire::format_clock;

/// The lane id of a requester seen only by its address.
const PEER_LANE_PREFIX: &str = "http-client ";

/// `doc` with the HTTP exchanges of `entries` drawn on it: the rows, and any
/// lane they need that the SIP projection did not declare.
pub fn with_rows(mut doc: SeqDoc, entries: &[RecordedHttpEntry], rec_lanes: &[RecLane]) -> SeqDoc {
    for e in entries {
        let service = ensure_lane(&mut doc, &e.service, LaneKind::Service);
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

/// The per-service wire views of the text report, keyed by relative path
/// (`service/<name>.txt`): every exchange a service lane took, request and
/// reply as sent.
pub fn service_views(
    entries: &[RecordedHttpEntry],
    rec_lanes: &[RecLane],
) -> Vec<(String, String)> {
    let base = entries.iter().map(|e| e.at_ms as i64).min().unwrap_or(0);
    rec_lanes
        .iter()
        .filter(|l| l.network == NetworkTag::Service)
        .filter_map(|lane| {
            let mine: Vec<&RecordedHttpEntry> =
                entries.iter().filter(|e| e.service == lane.key).collect();
            if mine.is_empty() {
                return None;
            }
            let name = lane.names.first().cloned().unwrap_or_else(|| lane.addr.to_string());
            let mut out = format!("HTTP exchanges served by {name} ({})\n\n", lane.addr);
            for e in mine {
                let from = e
                    .requester
                    .clone()
                    .or_else(|| e.peer.map(|p| p.to_string()))
                    .unwrap_or_else(|| "?".to_string());
                out.push_str(&format!(
                    "── [T+{}] {from} → {name} ── {} {}\n",
                    format_clock(e.at_ms as i64 - base),
                    e.request.method,
                    e.request.path
                ));
                out.push_str(&head_and_body(&e.request.headers, &e.request.body));
                let at = e.reply_at_ms.map(|t| format!("T+{}", format_clock(t as i64 - base)));
                let at = at.unwrap_or_else(|| "open".to_string());
                match &e.outcome {
                    Some(HttpOutcome::Response(resp)) => {
                        out.push_str(&format!("── [{at}] {name} → {from} ── {}\n", resp.status));
                        out.push_str(&head_and_body(&resp.headers, &resp.body));
                    }
                    Some(HttpOutcome::Abort) => {
                        out.push_str(&format!("── [{at}] {name} ✗ reset, no response\n\n"));
                    }
                    Some(HttpOutcome::Abandoned) | Some(HttpOutcome::Error(_)) | None => {
                        out.push_str(&format!("── [{at}] {name} ✗ no reply\n\n"));
                    }
                }
            }
            Some((format!("service/{}.txt", slug(&name)), out))
        })
        .collect()
}

/// The report anomalies of a scripted service's findings: gating unless the
/// finding is advisory, never RFC-rule-sourced, each linked to the request row
/// it refused (the served `500` answering the same request).
pub fn verdict_anomalies(findings: &[HttpFinding], entries: &[RecordedHttpEntry]) -> Vec<Anomaly> {
    let mut linked: HashSet<u64> = HashSet::new();
    findings
        .iter()
        .map(|f| {
            let row = entries.iter().find(|e| {
                !linked.contains(&e.seq)
                    && e.served
                    && e.request.method == f.method
                    && e.request.path == f.path
                    && String::from_utf8_lossy(&e.request.body) == f.body
                    && matches!(&e.outcome, Some(HttpOutcome::Response(r)) if r.status == 500)
            });
            if let Some(e) = row {
                linked.insert(e.seq);
            }
            let detail = if f.method.is_empty() {
                f.detail.clone()
            } else {
                format!("{} {}: {}", f.method, f.path, f.detail)
            };
            Anomaly {
                check: check_of(f.kind).to_string(),
                detail,
                lane: row.map(|e| e.service.clone()),
                endpoint: None,
                advisory: Some(f.is_advisory()),
                row_seqs: row.map(|e| vec![e.seq]).unwrap_or_default(),
                rule_sourced: false,
            }
        })
        .collect()
}

fn check_of(kind: HttpFindingKind) -> &'static str {
    match kind {
        HttpFindingKind::Unmatched => "http.unmatched",
        HttpFindingKind::Ambiguous => "http.ambiguous",
        HttpFindingKind::Unserved => "http.unserved",
        HttpFindingKind::ResetNotForwarded => "http.resetNotForwarded",
        HttpFindingKind::ForeignToken => "http.foreignToken",
    }
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

/// Headers, a blank line, and the body as sent.
fn head_and_body(headers: &[(String, String)], body: &[u8]) -> String {
    let mut out: String = headers.iter().map(|(k, v)| format!("{k}: {v}\n")).collect();
    out.push('\n');
    out.push_str(&String::from_utf8_lossy(body));
    out.push_str("\n\n");
    out
}

/// A file-name-safe form of a lane name.
fn slug(name: &str) -> String {
    name.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}
