//! SVG sequence-diagram renderer. Port of `renderSequenceDiagram` in
//! `svg-sequence-diagram.ts`, trimmed to the surface the harness drives today:
//! lanes are `(ip, port)` columns, arrows are placed on their `from`/`to`
//! addresses (never on a name string, so a fabricated name can't move an
//! arrow), captions are coloured by Call-ID, and timestamps are relative to
//! the first entry. Replication frames, kill bands, and dual-fabric banding
//! are dropped until the layers that produce them are ported (see
//! MIGRATION_STATUS.md).
//!
//! (Raw-string literals use `r##"..."##` because the XML colour attributes
//! contain the `"#` sequence that would otherwise close an `r#"..."#` string.)

use std::collections::HashMap;
use std::net::SocketAddr;

use layer_harness::{Lane, TransportKind};
use sip_net::RecordedSipEntry;

use super::wire::{facets, format_relative};

const PARTICIPANT_SPACING: i64 = 250;
const ROW_HEIGHT: i64 = 55;
const HEADER_HEIGHT: i64 = 80;
const BOX_W: i64 = 200;
const BOX_H: i64 = 56;
const MARGIN_LEFT: i64 = 40;
const MARGIN_TOP: i64 = 20;
const FONT: i64 = 12;
const LABEL_FONT: i64 = 11;
const ARROW: i64 = 8;

const CALL_ID_COLORS: [&str; 8] = [
    "#2563eb", "#dc2626", "#059669", "#7c3aed", "#d97706", "#0891b2", "#be185d", "#4f46e5",
];

fn transport_canvas(kind: TransportKind) -> &'static str {
    match kind {
        TransportKind::Fake => "#eef2ff",
        TransportKind::Live => "#ecfdf5",
        TransportKind::Hybrid => "#faf5ff",
    }
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn call_id_color<'a>(call_id: &str, map: &mut HashMap<String, &'a str>) -> &'a str {
    if let Some(c) = map.get(call_id) {
        return c;
    }
    let c = CALL_ID_COLORS[map.len() % CALL_ID_COLORS.len()];
    map.insert(call_id.to_string(), c);
    c
}

/// Render the trace as a standalone SVG document.
pub fn render(entries: &[RecordedSipEntry], lanes: &[Lane], transport_kind: TransportKind) -> String {
    if lanes.is_empty() || entries.is_empty() {
        return r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="100"><text x="200" y="50" text-anchor="middle" font-family="monospace" font-size="14" fill="#666">No messages to display</text></svg>"##.to_string();
    }

    let mut lane_x: HashMap<SocketAddr, i64> = HashMap::new();
    for (i, lane) in lanes.iter().enumerate() {
        lane_x.insert(lane.addr, MARGIN_LEFT + i as i64 * PARTICIPANT_SPACING + BOX_W / 2);
    }

    let mut rows: Vec<&RecordedSipEntry> = entries.iter().collect();
    rows.sort_by(|a, b| a.sent_ms.cmp(&b.sent_ms).then(a.seq.cmp(&b.seq)));
    let base_ts = rows[0].sent_ms as i64;

    let total_width = MARGIN_LEFT * 2 + (lanes.len() as i64 - 1) * PARTICIPANT_SPACING + BOX_W;
    let mut row_ys = Vec::new();
    let mut y = MARGIN_TOP + HEADER_HEIGHT;
    for _ in &rows {
        row_ys.push(y);
        y += ROW_HEIGHT;
    }
    let total_height = y + 40;

    let mut p: Vec<String> = Vec::new();
    p.push(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{total_width}" height="{total_height}" class="sip-diagram">"##
    ));
    let half = ARROW / 2;
    p.push(format!(
        r##"<defs>
  <marker id="arrowhead-pass" markerWidth="{ARROW}" markerHeight="{ARROW}" refX="{ARROW}" refY="{half}" orient="auto"><polygon points="0 0, {ARROW} {half}, 0 {ARROW}" fill="#374151"/></marker>
  <marker id="arrowhead-unexpected" markerWidth="{ARROW}" markerHeight="{ARROW}" refX="{ARROW}" refY="{half}" orient="auto"><polygon points="0 0, {ARROW} {half}, 0 {ARROW}" fill="#d97706"/></marker>
</defs>"##
    ));
    p.push(format!(
        r##"<rect width="{total_width}" height="{total_height}" fill="{}"/>"##,
        transport_canvas(transport_kind)
    ));

    for lane in lanes {
        let x = lane_x[&lane.addr];
        let box_x = x - BOX_W / 2;
        p.push(format!(
            r##"<rect x="{box_x}" y="{MARGIN_TOP}" width="{BOX_W}" height="{BOX_H}" rx="4" fill="#f3f4f6" stroke="#6b7280" stroke-width="1.5"/>"##
        ));
        p.push(format!(
            r##"<text x="{x}" y="{y1}" text-anchor="middle" font-family="monospace" font-size="{FONT}" font-weight="bold" fill="#111827">{label}</text>"##,
            y1 = MARGIN_TOP + 22,
            label = escape_xml(&lane.addr.to_string())
        ));
        let names = lane.names.join(", ");
        if !names.is_empty() {
            p.push(format!(
                r##"<text x="{x}" y="{y1}" text-anchor="middle" font-family="monospace" font-size="{fs}" fill="#4b5563">{names}</text>"##,
                y1 = MARGIN_TOP + 42,
                fs = FONT - 1,
                names = escape_xml(&names)
            ));
        }
        p.push(format!(
            r##"<line x1="{x}" y1="{y1}" x2="{x}" y2="{y2}" stroke="#d1d5db" stroke-width="1" stroke-dasharray="4,4"/>"##,
            y1 = MARGIN_TOP + BOX_H,
            y2 = total_height - 20
        ));
    }

    let mut color_map: HashMap<String, &str> = HashMap::new();
    for (idx, entry) in rows.iter().enumerate() {
        let (Some(&from_x), Some(&to_x)) = (lane_x.get(&entry.from), lane_x.get(&entry.to)) else {
            continue;
        };
        let ay = row_ys[idx];
        let f = facets(&entry.raw);
        let color = call_id_color(&f.call_id, &mut color_map).to_string();

        let left_to_right = from_x < to_x;
        let (line_color, marker, dash) = if entry.delivered {
            ("#374151", "url(#arrowhead-pass)", "")
        } else {
            ("#d97706", "url(#arrowhead-unexpected)", r#" stroke-dasharray="6,4""#)
        };
        let arrow_from = if left_to_right { from_x + 5 } else { from_x - 5 };
        let arrow_to = if left_to_right { to_x - 5 } else { to_x + 5 };

        p.push(format!(r##"<g class="trace-arrow" data-trace-index="{idx}">"##));
        p.push(format!(
            r##"<line x1="{arrow_from}" y1="{ay}" x2="{arrow_to}" y2="{ay}" stroke="{line_color}" stroke-width="1.5" marker-end="{marker}"{dash}/>"##
        ));
        let mid_x = (from_x + to_x) / 2;
        p.push(format!(
            r##"<text x="{mid_x}" y="{ly}" text-anchor="middle" font-family="monospace" font-size="{LABEL_FONT}" fill="{color}">{label}</text>"##,
            ly = ay - 14,
            label = escape_xml(&f.label)
        ));
        if !f.tag_label.is_empty() {
            p.push(format!(
                r##"<text x="{mid_x}" y="{ly}" text-anchor="middle" font-family="monospace" font-size="{fs}" fill="#9ca3af">{tag}</text>"##,
                ly = ay - 3,
                fs = LABEL_FONT - 2,
                tag = escape_xml(&f.tag_label)
            ));
        }
        let rcvd = entry.received_ms.unwrap_or(entry.sent_ms) as i64 - base_ts;
        let recv_anchor = if left_to_right { "start" } else { "end" };
        let recv_x = if left_to_right { to_x + 8 } else { to_x - 8 };
        p.push(format!(
            r##"<text x="{recv_x}" y="{ly}" text-anchor="{recv_anchor}" font-family="monospace" font-size="{fs}" fill="#6b7280">{ts}</text>"##,
            ly = ay + 4,
            fs = LABEL_FONT - 2,
            ts = format_relative(rcvd)
        ));
        if !entry.delivered {
            let badge_x = if left_to_right { to_x + 8 } else { to_x - 70 };
            p.push(format!(
                r##"<text x="{badge_x}" y="{ly}" text-anchor="start" font-family="monospace" font-size="{fs}" font-weight="bold" fill="#d97706">UNDELIVERED</text>"##,
                ly = ay + 4,
                fs = LABEL_FONT - 1
            ));
        }
        p.push("</g>".to_string());
    }

    p.push("</svg>".to_string());
    p.join("\n")
}
