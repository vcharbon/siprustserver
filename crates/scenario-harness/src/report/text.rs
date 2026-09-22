//! Text report writer — port of `text-report.ts`.
//!
//! The **global** view (`<name>.global.txt`) is now produced by the SHARED
//! [`seq_report`] renderer over a single-plane [`seq_report::SeqDoc`] (the
//! unification described in `seq-report`'s crate docs), so it is the same
//! timeline format the failover harness uses for its three-plane view.
//!
//! The **per-endpoint** views (`<net>/<agent>.txt`, one per agent filtered to
//! that agent's wire address) keep the historic per-message wire dump here —
//! they are a SIP-specific, single-actor cut with no analogue in the neutral
//! model, so they stay native to scenario-harness. Lane identity is `(ip,port)`;
//! names are decorations resolved from the lane registry.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use http_net::{HttpOutcome, RecordedHttpEntry};
use layer_harness::{Lane, NetworkTag, RecordedScenario, TransportKind};
use seq_report::Anomaly;
use sip_net::RecordedSipEntry;

use super::wire::{facets, format_clock, wire_text};

const SEP_WIDTH: usize = 80;

fn name_by_addr(lanes: &[Lane]) -> BTreeMap<SocketAddr, String> {
    let mut idx = BTreeMap::new();
    for lane in lanes {
        idx.insert(lane.addr, lane.names.first().cloned().unwrap_or_default());
    }
    idx
}

fn endpoint_label(names: &BTreeMap<SocketAddr, String>, addr: &SocketAddr) -> String {
    match names.get(addr) {
        Some(n) if !n.is_empty() => format!("{n} ({addr})"),
        _ => addr.to_string(),
    }
}

fn render_entries(
    entries: &[&RecordedSipEntry],
    base_ts: i64,
    names: &BTreeMap<SocketAddr, String>,
) -> String {
    let mut sorted: Vec<&&RecordedSipEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| a.sent_ms.cmp(&b.sent_ms).then(a.seq.cmp(&b.seq)));

    let mut lines: Vec<String> = Vec::new();
    for entry in sorted {
        let sent_rel = entry.sent_ms as i64 - base_ts;
        let rcvd_rel = entry.received_ms.unwrap_or(entry.sent_ms) as i64 - base_ts;
        let ts_block = if entry.received_ms.is_none() || entry.received_ms == Some(entry.sent_ms) {
            format!("T+{}", format_clock(sent_rel))
        } else {
            format!("sent T+{} → rcvd T+{}", format_clock(sent_rel), format_clock(rcvd_rel))
        };
        let label = facets(&entry.raw).label;
        let status_tag = if entry.delivered { "" } else { " [UNDELIVERED]" };
        let from = endpoint_label(names, &entry.from);
        let to = endpoint_label(names, &entry.to);
        let prefix = format!("── [{ts_block}] {from} → {to} ── {label}{status_tag} ");
        let mut padded = prefix.clone();
        while padded.chars().count() < SEP_WIDTH {
            padded.push('─');
        }
        lines.push(padded);
        lines.push(String::new());
        lines.push(wire_text(&entry.raw));
        lines.push(String::new());
    }
    lines.join("\n")
}

fn render_header(
    scenario_name: &str,
    view_label: &str,
    transport_kind: TransportKind,
    passed: bool,
    description: Option<&str>,
) -> String {
    let status = if passed { "PASS" } else { "FAIL" };
    let transport = match transport_kind {
        TransportKind::Fake => "FAKE NET",
        TransportKind::Live => "LIVE UDP",
        TransportKind::Hybrid => "HYBRID",
    };
    let mut lines = vec![
        "=".repeat(SEP_WIDTH),
        format!("  SIP Exchange Report: {scenario_name}"),
        format!("  View: {view_label}"),
        format!("  Transport: {transport}"),
        format!("  Status: {status}"),
        "=".repeat(SEP_WIDTH),
        String::new(),
    ];
    if let Some(desc) = description.map(str::trim).filter(|d| !d.is_empty()) {
        lines.push("Description:".to_string());
        lines.push(String::new());
        for raw in desc.split('\n') {
            lines.push(if raw.is_empty() { String::new() } else { format!("  {raw}") });
        }
        lines.push(String::new());
        lines.push("-".repeat(SEP_WIDTH));
        lines.push(String::new());
    }
    lines.push(String::new());
    lines.join("\n")
}

/// The rendered text views, keyed by relative file path (e.g.
/// `"<name>.global.txt"`, `"ext/<agent>.txt"`).
pub struct TextReports {
    pub files: BTreeMap<String, String>,
}

/// Render the global + per-endpoint + per-service text views. Pure — call
/// [`TextReports::write_to`] to materialise them on disk.
pub fn render(
    scenario_name: &str,
    description: Option<&str>,
    entries: &[RecordedSipEntry],
    http: &[RecordedHttpEntry],
    scenario: &RecordedScenario,
    passed: bool,
    extra_anomalies: &[Anomaly],
) -> TextReports {
    let names = name_by_addr(&scenario.lanes);
    let base_ts = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);
    let mut files = BTreeMap::new();

    // Global view — the SHARED unified renderer over a single-plane SeqDoc.
    let doc = super::project::sip_doc(
        scenario_name,
        description,
        entries,
        scenario,
        passed,
        extra_anomalies,
    );
    let doc = super::http::with_rows(doc, http, &scenario.lanes);
    files.insert(format!("{scenario_name}.global.txt"), seq_report::render_global_txt(&doc));

    // Per-service views — the HTTP exchanges each recorded service took, on the
    // global view's timeline.
    let doc_base = doc.rows.iter().map(|r| r.at_ms).min().unwrap_or(0);
    files.extend(service_views(http, &scenario.lanes, doc_base));

    // Per-endpoint views — one per lane that sent or received a message.
    for lane in &scenario.lanes {
        let filtered: Vec<&RecordedSipEntry> =
            entries.iter().filter(|e| e.from == lane.addr || e.to == lane.addr).collect();
        if filtered.is_empty() {
            continue;
        }
        let slug =
            lane.names.first().cloned().unwrap_or_else(|| lane.addr.to_string().replace(':', "-"));
        let net = match lane.network {
            layer_harness::NetworkTag::Ext => "ext",
            layer_harness::NetworkTag::Core => "core",
            layer_harness::NetworkTag::Service => "service",
        };
        let view_label = match lane.names.first() {
            Some(n) => format!("{n} (endpoint, network={net})"),
            None => format!("{} (endpoint, network={net})", lane.addr),
        };
        let header =
            render_header(scenario_name, &view_label, scenario.transport_kind, passed, description);
        let body = render_entries(&filtered, base_ts, &names);
        files.insert(format!("{net}/{slug}.txt"), format!("{header}{body}"));
    }

    TextReports { files }
}

/// The per-service wire views of the text report, keyed by relative path
/// (`service/<name>.txt`): every exchange a service lane took, request and
/// reply as sent, stamped from `base` (the run's timeline start).
fn service_views(
    entries: &[RecordedHttpEntry],
    rec_lanes: &[Lane],
    base: i64,
) -> Vec<(String, String)> {
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

impl TextReports {
    /// Write every view under `out_dir`, creating `ext/` / `core/`
    /// subfolders as needed. Returns the absolute paths written.
    pub fn write_to(&self, out_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
        let mut written = Vec::new();
        for (rel, content) in &self.files {
            let path = out_dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content)?;
            written.push(path);
        }
        Ok(written)
    }
}
