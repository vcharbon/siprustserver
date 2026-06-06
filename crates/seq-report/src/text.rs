//! The `global.txt` companion — a plain-text rendering of the unified timeline.
//!
//! One line per row, in `(at_ms, seq)` order, tagged with the plane so SIP,
//! replication, and lifecycle events are distinguishable in a terminal. A
//! lifecycle row renders as a centred `=== … ===` band (matching the historic
//! ha-harness text band). Message rows carry their detail (full wire text for
//! SIP) indented under the line so the file is self-contained.

use crate::{format_relative, RowKind, SeqDoc, SeqRow};

const SEP_WIDTH: usize = 80;

/// Render the whole [`SeqDoc`] as the unified `global.txt`.
pub fn render_global_txt(doc: &SeqDoc) -> String {
    let base = doc.base_ms();
    let mut out = String::new();

    // Header.
    out.push_str(&"=".repeat(SEP_WIDTH));
    out.push('\n');
    out.push_str(&format!("  Unified sequence: {}\n", doc.title));
    out.push_str(&format!("  Status: {}\n", if doc.passed { "PASS" } else { "FAIL" }));
    out.push_str(&format!("  Lanes: {}\n", lane_line(doc)));
    out.push_str(&format!(
        "  Rows: {}  (sip={}, repl={}, lifecycle={})\n",
        doc.rows.len(),
        count(doc, |k| matches!(k, RowKind::Sip { .. })),
        count(doc, |k| matches!(k, RowKind::Repl { .. })),
        count(doc, |k| matches!(k, RowKind::Lifecycle)),
    ));
    out.push_str(&"=".repeat(SEP_WIDTH));
    out.push('\n');
    if let Some(desc) = doc.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
        out.push('\n');
        for line in desc.split('\n') {
            out.push_str(&format!("  {line}\n"));
        }
    }
    out.push_str(&"-".repeat(SEP_WIDTH));
    out.push('\n');

    // Legend so the plane tags are self-describing.
    out.push_str("  legend: [SIP] request/response · [REPL] replication frame · === lifecycle ===\n");
    out.push_str(&"-".repeat(SEP_WIDTH));
    out.push('\n');

    for row in doc.sorted_rows() {
        render_row(&mut out, row, base, doc);
    }

    if !doc.anomalies.is_empty() {
        out.push('\n');
        out.push_str(&"-".repeat(SEP_WIDTH));
        out.push('\n');
        out.push_str(&format!("Anomalies ({}):\n", doc.anomalies.len()));
        for a in &doc.anomalies {
            let lane = a.lane.as_deref().map(|l| format!(" [{l}]")).unwrap_or_default();
            out.push_str(&format!("  • {}{lane}: {}\n", a.check, a.detail));
        }
    }

    out
}

fn render_row(out: &mut String, row: &SeqRow, base: i64, doc: &SeqDoc) {
    let ts = format_relative(row.at_ms - base);
    match row.kind {
        RowKind::Lifecycle => {
            // A centred full-width band, stamped with its time at the left.
            let core = format!(" {} ", row.label);
            let stamp = format!("[{ts}] ");
            let room = SEP_WIDTH.saturating_sub(stamp.chars().count() + core.chars().count());
            let left = room / 2;
            let right = room - left;
            out.push('\n');
            out.push_str(&format!(
                "{stamp}{}==={core}==={}\n",
                "=".repeat(left.max(1)),
                "=".repeat(right.max(1)),
            ));
            out.push('\n');
        }
        RowKind::Sip { delivered } | RowKind::Repl { delivered } => {
            let plane = match row.kind {
                RowKind::Sip { .. } => "SIP ",
                RowKind::Repl { .. } => "REPL",
                RowKind::Lifecycle => unreachable!(),
            };
            let from = lane_label(doc, &row.from);
            let to = row
                .to
                .as_deref()
                .map(|t| lane_label(doc, t))
                .unwrap_or_else(|| "?".into());
            // The socket tag disambiguates which connection a repl frame rode —
            // so a frame "lost to b2" is legible as a different (defunct) socket
            // than the live one collapsed on the same lane.
            let conn = row.conn.as_deref().map(|c| format!(" {c}")).unwrap_or_default();
            let undelivered = if delivered { "" } else { "  ✗ [LOST IN TRANSIT]" };
            out.push_str(&format!(
                "[{ts}] [{plane}] {from} -> {to}  {}{conn}{undelivered}\n",
                row.label,
            ));
            if let Some(detail) = row.detail.as_deref().filter(|d| !d.trim().is_empty()) {
                for line in detail.split('\n') {
                    out.push_str(&format!("        {line}\n"));
                }
            }
        }
    }
}

fn count(doc: &SeqDoc, pred: impl Fn(RowKind) -> bool) -> usize {
    doc.rows.iter().filter(|r| pred(r.kind)).count()
}

fn lane_line(doc: &SeqDoc) -> String {
    doc.lanes
        .iter()
        .map(|l| l.id.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve a lane id to its caption, falling back to the id for an unknown lane
/// (a projector may reference a lane it did not declare — render it raw rather
/// than panic).
fn lane_label(doc: &SeqDoc, id: &str) -> String {
    doc.lanes
        .iter()
        .find(|l| l.id == id)
        .map(|l| l.label.clone())
        .unwrap_or_else(|| id.to_string())
}
