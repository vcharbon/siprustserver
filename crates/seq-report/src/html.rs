//! Self-contained HTML rendering of the unified timeline.
//!
//! Produces a single static document (no external assets) with a two-pane,
//! viewport-filling layout that mirrors the proven sipjs report design:
//!   - a header (title / status / anomaly count),
//!   - a **legend** naming the three planes,
//!   - a `.main` flex row that fills the viewport:
//!     - LEFT `.diagram-panel` — the scrollable inline-SVG sequence diagram with
//!       one lifeline per lane, SIP arrows in one colour, replication arrows in
//!       another (dashed), and lifecycle events as full-width labelled bands.
//!       Each SIP/REPL message is a clickable `<g class="seq-msg" data-idx="N">`
//!       group containing the line + label + a transparent full-row hit `<rect>`
//!       so the whole row is clickable.
//!     - RIGHT `.detail-panel` — a FIXED detail panel (`Message Detail`) whose
//!       scrollable `.detail-body` shows the full payload of the clicked message.
//!   - the anomalies list, under the diagram panel.
//!
//! ## Payload carrying (robust — no JS string escaping)
//! Per-message payloads are kept as HIDDEN, HTML-escaped blocks in the DOM:
//! `<div class="payload" id="evt-{N}" hidden><pre>…escaped wire text…</pre></div>`.
//! A small `<script>` wires each `.seq-msg` click to copy its matching
//! `#evt-{N}` block's `innerHTML` into the `.detail-body`. This reuses the
//! already-escaped payload, needs no JS string escaping, and is `</script>`-safe.
//!
//! The diagram loop and the hidden-payload loop iterate the IDENTICAL
//! `doc.sorted_rows()` slice in lockstep, so the diagram `data-idx="{N}"` and the
//! payload `id="evt-{N}"` always derive from the same ordinal.
//!
//! The SVG is laid out by lane INDEX (x) and row ORDINAL (y) — rows are equally
//! spaced rather than scaled by time, so a long quiescent gap does not blow up
//! the page; the relative `T+…` stamp on each row carries the actual timing.

use crate::{format_relative, Lane, LaneKind, RowKind, SeqDoc, SeqRow};

// SVG layout constants.
const LANE_GAP: i64 = 150;
const LEFT_PAD: i64 = 90;
const TOP_PAD: i64 = 70;
const ROW_GAP: i64 = 46;
const BOTTOM_PAD: i64 = 30;

const SIP_COLOR: &str = "#2563eb"; // blue
const REPL_COLOR: &str = "#9333ea"; // purple
const BAND_COLOR: &str = "#b91c1c"; // red

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn lane_x(idx: usize) -> i64 {
    LEFT_PAD + idx as i64 * LANE_GAP
}

fn lane_color(kind: LaneKind) -> &'static str {
    match kind {
        LaneKind::Ua => "#0f766e",
        LaneKind::Sut => "#92400e",
        LaneKind::Node => "#1e3a8a",
    }
}

/// Render the whole [`SeqDoc`] as one HTML document string.
pub fn render_html(doc: &SeqDoc) -> String {
    let rows = doc.sorted_rows();
    let base = doc.base_ms();
    let lane_idx: std::collections::HashMap<&str, usize> = doc
        .lanes
        .iter()
        .enumerate()
        .map(|(i, l)| (l.id.as_str(), i))
        .collect();

    let svg = render_svg(doc, &rows, base, &lane_idx);
    let payloads = render_payloads(doc, &rows, base);
    let anomalies = render_anomalies(doc);

    let status = if doc.passed { "PASS" } else { "FAIL" };
    let status_color = if doc.passed { "#059669" } else { "#dc2626" };
    let desc = doc
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!("<p class=\"desc\">{}</p>", escape(d)))
        .unwrap_or_default();

    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<title>Unified sequence — {title}</title>
<style>
  * {{ box-sizing: border-box; }}
  body {{ font-family: system-ui, sans-serif; color: #111827; margin: 0;
         display: flex; flex-direction: column; height: 100vh; }}
  header {{ padding: 12px 20px; border-bottom: 1px solid #e5e7eb; }}
  h1 {{ font-size: 1.25rem; margin: 0; }}
  .status {{ font-weight: bold; color: {status_color}; }}
  .desc {{ color: #4b5563; margin: 0.25rem 0 0; }}
  .legend {{ margin: 0.5rem 0 0; font-size: 0.9rem; }}
  .legend span {{ margin-right: 1.25rem; }}
  .swatch {{ display: inline-block; width: 1.4rem; height: 0; vertical-align: middle;
            border-top-width: 3px; border-top-style: solid; margin-right: 0.35rem; }}
  /* Two-pane viewport-filling layout: scrollable diagram on the left, FIXED
     always-visible message-detail panel on the right. */
  .main {{ display: flex; flex: 1; overflow: hidden; }}
  .diagram-panel {{ flex: 1; overflow: auto; padding: 20px; }}
  .diagram-panel svg {{ display: block; }}
  .detail-panel {{ width: 500px; border-left: 1px solid #e5e7eb; background: #ffffff;
                  display: flex; flex-direction: column; overflow: hidden; }}
  .detail-header {{ padding: 12px 16px; background: #f3f4f6; border-bottom: 1px solid #e5e7eb;
                   font-size: 13px; font-weight: 600; color: #374151; }}
  .detail-body {{ flex: 1; overflow: auto; padding: 16px; }}
  .detail-placeholder {{ color: #9ca3af; font-style: italic; padding: 20px; text-align: center; }}
  .payload-head {{ margin-bottom: 8px; }}
  .seq-sip .payload-head {{ color: {SIP_COLOR}; }}
  .seq-repl .payload-head {{ color: {REPL_COLOR}; }}
  .ts {{ color: #6b7280; font-family: monospace; }}
  /* Clickable diagram messages: hover thickens the arrow + tints the hit row;
     the selected row stays tinted. */
  .seq-msg:hover line {{ stroke-width: 3; }}
  .seq-msg:hover text {{ text-decoration: underline; }}
  .seq-msg:hover rect {{ fill: rgba(37, 99, 235, 0.05); }}
  .seq-msg.selected rect {{ fill: rgba(37, 99, 235, 0.12); }}
  /* Hidden payload blocks: the click handler copies these into `.detail-body`.
     The `<pre>` shows the FULL content with no inner scrollbar / no height
     clamp — `white-space: pre-wrap` + `overflow-wrap: anywhere` wrap long header
     lines instead of forcing a horizontal scrollbar; the `.detail-body` itself
     scrolls if the payload is very long. */
  .payload {{ display: none; }}
  pre {{ background: #f9fafb; padding: 8px; border-radius: 4px; margin: 4px 0;
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
        white-space: pre-wrap; overflow-wrap: anywhere; overflow: visible;
        max-height: none; }}
  .anomalies {{ padding: 12px 20px; border-top: 1px solid #e5e7eb; }}
</style></head>
<body>
  <header>
    <h1>Unified sequence: {title}</h1>
    <p>Status: <span class="status">{status}</span> &middot; {anomaly_count} anomalies recorded</p>
    {desc}
    <div class="legend">
      <span><i class="swatch" style="border-top-color:{SIP_COLOR}"></i>SIP</span>
      <span><i class="swatch" style="border-top-color:{REPL_COLOR};border-top-style:dashed"></i>Replication</span>
      <span><i class="swatch" style="border-top-color:{BAND_COLOR}"></i>Lifecycle (crash / reboot / failover / partition)</span>
    </div>
  </header>
  <div class="main">
    <div class="diagram-panel">{svg}{anomalies}</div>
    <div class="detail-panel">
      <div class="detail-header">Message Detail</div>
      <div class="detail-body">
        <div class="detail-placeholder">Click a message to inspect</div>
      </div>
    </div>
  </div>
  <!-- Hidden, already-HTML-escaped payload blocks, one per diagram message. The
       click handler copies the matching `#evt-{{N}}` innerHTML into .detail-body. -->
  {payloads}
  <script>
    document.querySelectorAll('.seq-msg').forEach(g => g.addEventListener('click', () => {{
      document.querySelectorAll('.seq-msg.selected').forEach(s => s.classList.remove('selected'));
      g.classList.add('selected');
      const src = document.getElementById('evt-' + g.dataset.idx);
      document.querySelector('.detail-body').innerHTML = src ? src.innerHTML
          : '<div class="detail-placeholder">No payload recorded for this message</div>';
    }}));
  </script>
</body></html>"#,
        title = escape(&doc.title),
        anomaly_count = doc.anomalies.len(),
    )
}

fn render_svg(
    doc: &SeqDoc,
    rows: &[&SeqRow],
    base: i64,
    lane_idx: &std::collections::HashMap<&str, usize>,
) -> String {
    let n_lanes = doc.lanes.len().max(1);
    let width = LEFT_PAD + (n_lanes as i64) * LANE_GAP;
    let height = TOP_PAD + (rows.len() as i64) * ROW_GAP + BOTTOM_PAD;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" font-family=\"system-ui, sans-serif\" font-size=\"12\">\n"
    ));
    // Arrowhead markers, one per message plane colour.
    s.push_str(&format!(
        "<defs>\
<marker id=\"ah-sip\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"3\" orient=\"auto\"><path d=\"M0,0 L7,3 L0,6 Z\" fill=\"{SIP_COLOR}\"/></marker>\
<marker id=\"ah-repl\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"3\" orient=\"auto\"><path d=\"M0,0 L7,3 L0,6 Z\" fill=\"{REPL_COLOR}\"/></marker>\
</defs>\n"
    ));

    // Lifelines + column heads.
    let life_bottom = height - BOTTOM_PAD / 2;
    for (i, lane) in doc.lanes.iter().enumerate() {
        let x = lane_x(i);
        let color = lane_color(lane.kind);
        s.push_str(&format!(
            "<line x1=\"{x}\" y1=\"{TOP_PAD}\" x2=\"{x}\" y2=\"{life_bottom}\" stroke=\"#d1d5db\" stroke-width=\"1\"/>\n"
        ));
        s.push_str(&format!(
            "<text x=\"{x}\" y=\"{}\" text-anchor=\"middle\" font-weight=\"bold\" fill=\"{color}\">{}</text>\n",
            TOP_PAD - 35,
            escape(&lane.label),
        ));
    }

    // Rows. `ord` here MUST match the payload-block ordinal in `render_payloads`
    // — both iterate the same sorted `rows` slice in lockstep, so index equality
    // ties a diagram `.seq-msg` to its `#evt-{ord}` payload.
    for (ord, row) in rows.iter().enumerate() {
        let y = TOP_PAD + (ord as i64) * ROW_GAP + ROW_GAP / 2;
        let ts = format_relative(row.at_ms - base);
        match row.kind {
            RowKind::Lifecycle => {
                // Full-width band — not clickable, carries no payload.
                s.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"22\" fill=\"#fee2e2\" stroke=\"{BAND_COLOR}\" stroke-dasharray=\"3 2\"/>\n",
                    LEFT_PAD - 50,
                    y - 11,
                    width - (LEFT_PAD - 50) - 10,
                ));
                s.push_str(&format!(
                    "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" fill=\"{BAND_COLOR}\" font-weight=\"bold\">⏻ {} &#160; ({})</text>\n",
                    width / 2,
                    y + 4,
                    escape(&row.label),
                    escape(&ts),
                ));
            }
            RowKind::Sip { delivered } | RowKind::Repl { delivered } => {
                let (color, marker, dash) = match row.kind {
                    RowKind::Sip { .. } => (SIP_COLOR, "ah-sip", ""),
                    RowKind::Repl { .. } => (REPL_COLOR, "ah-repl", " stroke-dasharray=\"5 3\""),
                    RowKind::Lifecycle => unreachable!(),
                };
                let plane_class = match row.kind {
                    RowKind::Sip { .. } => "seq-sip",
                    RowKind::Repl { .. } => "seq-repl",
                    RowKind::Lifecycle => unreachable!(),
                };
                let fi = lane_idx.get(row.from.as_str()).copied().unwrap_or(0);
                let ti = row
                    .to
                    .as_deref()
                    .and_then(|t| lane_idx.get(t).copied())
                    .unwrap_or(fi);
                let (x1, x2) = (lane_x(fi), lane_x(ti));
                let opacity = if delivered { "1" } else { "0.4" };
                // Each message is a clickable `<g class="seq-msg" data-idx="{ord}">`
                // whose payload lives in the hidden `#evt-{ord}` block. A trailing
                // transparent full-row `<rect>` makes the whole row clickable.
                s.push_str(&format!(
                    "<g class=\"seq-msg {plane_class}\" data-idx=\"{ord}\" style=\"cursor:pointer\">\n"
                ));
                if x1 == x2 {
                    // Self-message: a small loop tag at the lane.
                    s.push_str(&format!(
                        "<text x=\"{}\" y=\"{}\" fill=\"{color}\" opacity=\"{opacity}\">{} {}</text>\n",
                        x1 + 6,
                        y,
                        escape(&row.label),
                        if delivered { "" } else { "⚠" },
                    ));
                } else {
                    s.push_str(&format!(
                        "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{x2}\" y2=\"{y}\" stroke=\"{color}\" stroke-width=\"1.5\" opacity=\"{opacity}\" marker-end=\"url(#{marker})\"{dash}/>\n"
                    ));
                    let mid = (x1 + x2) / 2;
                    let und = if delivered { "" } else { " ⚠" };
                    s.push_str(&format!(
                        "<text x=\"{mid}\" y=\"{}\" text-anchor=\"middle\" fill=\"{color}\" opacity=\"{opacity}\">{}{und}</text>\n",
                        y - 4,
                        escape(&row.label),
                    ));
                }
                // The timestamp in the left gutter.
                s.push_str(&format!(
                    "<text x=\"6\" y=\"{}\" fill=\"#6b7280\" font-family=\"monospace\" font-size=\"10\">{}</text>\n",
                    y + 3,
                    escape(&ts),
                ));
                // Transparent full-row hit target so the whole row is clickable.
                let (rx, rw) = if x1 <= x2 { (x1, x2 - x1) } else { (x2, x1 - x2) };
                let rw = (rw + LANE_GAP).max(LANE_GAP);
                s.push_str(&format!(
                    "<rect x=\"{}\" y=\"{}\" width=\"{rw}\" height=\"{ROW_GAP}\" fill=\"transparent\"/>\n",
                    rx - LANE_GAP / 2,
                    y - ROW_GAP / 2,
                ));
                s.push_str("</g>\n");
            }
        }
    }

    s.push_str("</svg>\n");
    s
}

/// Build the hidden, HTML-escaped payload blocks — one per row, keyed `evt-{ord}`
/// so the diagram's `.seq-msg[data-idx={ord}]` click handler can copy it into the
/// detail panel. `ord` MUST match the diagram ordinal in `render_svg` (both
/// iterate the same sorted `rows` slice in lockstep). Lifecycle bands get no
/// payload block (they are not clickable `.seq-msg` groups).
fn render_payloads(doc: &SeqDoc, rows: &[&SeqRow], base: i64) -> String {
    let mut out = String::new();
    for (ord, row) in rows.iter().enumerate() {
        let ts = format_relative(row.at_ms - base);
        match row.kind {
            RowKind::Lifecycle => {}
            RowKind::Sip { delivered } | RowKind::Repl { delivered } => {
                let class = match row.kind {
                    RowKind::Sip { .. } => "seq-sip",
                    RowKind::Repl { .. } => "seq-repl",
                    RowKind::Lifecycle => unreachable!(),
                };
                let plane = match row.kind {
                    RowKind::Sip { .. } => "SIP",
                    RowKind::Repl { .. } => "REPL",
                    RowKind::Lifecycle => unreachable!(),
                };
                let from = lane_caption(doc, &row.from);
                let to = row
                    .to
                    .as_deref()
                    .map(|t| lane_caption(doc, t))
                    .unwrap_or_else(|| "?".into());
                let badge = if delivered { "" } else { " ⚠ UNDELIVERED" };
                let body = match row.detail.as_deref().filter(|d| !d.trim().is_empty()) {
                    Some(d) => format!("<pre>{}</pre>", escape(d)),
                    None => "<div class=\"detail-placeholder\">No payload recorded for this message</div>".to_string(),
                };
                out.push_str(&format!(
                    "<div class=\"payload {class}\" id=\"evt-{ord}\" hidden><div class=\"payload-head\"><code>{from} → {to}</code> &nbsp; <b>[{plane}] {}</b> &nbsp; <span class=\"ts\">{}</span>{badge}</div>{body}</div>\n",
                    escape(&row.label),
                    escape(&ts),
                ));
            }
        }
    }
    out
}

fn render_anomalies(doc: &SeqDoc) -> String {
    if doc.anomalies.is_empty() {
        return String::new();
    }
    let mut out = String::from("<div class=\"anomalies\"><h2>Anomalies</h2>\n<ul>\n");
    for a in &doc.anomalies {
        let lane = a.lane.as_deref().map(|l| format!(" [{}]", escape(l))).unwrap_or_default();
        out.push_str(&format!(
            "<li><code>{}</code>{lane}: {}</li>\n",
            escape(&a.check),
            escape(&a.detail),
        ));
    }
    out.push_str("</ul></div>\n");
    out
}

fn lane_caption(doc: &SeqDoc, id: &str) -> String {
    doc.lanes
        .iter()
        .find(|l: &&Lane| l.id == id)
        .map(|l| escape(&l.label))
        .unwrap_or_else(|| escape(id))
}
