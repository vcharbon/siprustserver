//! Text report writer — port of `text-report.ts`. Produces a global view (all
//! exchanges) and one per-endpoint view filtered to a single agent's wire
//! address, each with full wire text per message. Lane identity is `(ip,port)`;
//! names are decorations resolved from the lane registry.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use layer_harness::{Lane, RecordedScenario, TransportKind};
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

/// Render the global + per-endpoint text views. Pure — call
/// [`TextReports::write_to`] to materialise them on disk.
pub fn render(
    scenario_name: &str,
    description: Option<&str>,
    entries: &[RecordedSipEntry],
    scenario: &RecordedScenario,
    passed: bool,
) -> TextReports {
    let names = name_by_addr(&scenario.lanes);
    let base_ts = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);
    let mut files = BTreeMap::new();

    // Global view.
    let all: Vec<&RecordedSipEntry> = entries.iter().collect();
    let header = render_header(scenario_name, "Global (all endpoints)", scenario.transport_kind, passed, description);
    let body = render_entries(&all, base_ts, &names);
    files.insert(format!("{scenario_name}.global.txt"), format!("{header}{body}"));

    // Per-endpoint views — one per lane that sent or received a message.
    for lane in &scenario.lanes {
        let filtered: Vec<&RecordedSipEntry> = entries
            .iter()
            .filter(|e| e.from == lane.addr || e.to == lane.addr)
            .collect();
        if filtered.is_empty() {
            continue;
        }
        let slug = lane
            .names
            .first()
            .cloned()
            .unwrap_or_else(|| lane.addr.to_string().replace(':', "-"));
        let net = match lane.network {
            layer_harness::NetworkTag::Ext => "ext",
            layer_harness::NetworkTag::Core => "core",
        };
        let view_label = match lane.names.first() {
            Some(n) => format!("{n} (endpoint, network={net})"),
            None => format!("{} (endpoint, network={net})", lane.addr),
        };
        let header = render_header(scenario_name, &view_label, scenario.transport_kind, passed, description);
        let body = render_entries(&filtered, base_ts, &names);
        files.insert(format!("{net}/{slug}.txt"), format!("{header}{body}"));
    }

    TextReports { files }
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
