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
//!   - the anomalies list, under the diagram panel — severity-ordered (gating
//!     first, badge per finding, split counted in the header). A finding whose
//!     [`Anomaly::row_seqs`] resolve to diagram rows is *linked*: clicking it
//!     highlights those rows, scrolls to the first, and opens its payload; the
//!     linked rows carry a ⚠ badge and their payload blocks embed the finding,
//!     so the message↔anomaly join reads from either end.
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

use std::collections::HashMap;

use crate::views::{disagreements, views_table, ViewChange};
use crate::{format_relative, Anomaly, Item, Lane, LaneKind, RowKind, SeqDoc};

// SVG layout constants.
const LANE_GAP: i64 = 150;
const LEFT_PAD: i64 = 90;
const TOP_PAD: i64 = 70;
const ROW_GAP: i64 = 46;
const BOTTOM_PAD: i64 = 30;

const SIP_COLOR: &str = "#2563eb"; // blue
const REPL_COLOR: &str = "#9333ea"; // purple
const BAND_COLOR: &str = "#b91c1c"; // red
const LOST_COLOR: &str = "#dc2626"; // red — the "✗ lost in transit" cross
const GATING_COLOR: &str = "#dc2626"; // red — gating-anomaly badge
const ADVISORY_COLOR: &str = "#d97706"; // amber — advisory-anomaly badge
const VIEW_COLOR: &str = "#4338ca"; // indigo — a belief change on the views plane
const VIEW_FILL: &str = "#eef2ff"; // indigo tint — the belief-change chip
const DISPUTE_FILL: &str = "#fef3c7"; // amber tint — observers disagree here

/// Categorical palette for per-socket coloring of replication arrows. Each
/// distinct connection (ephemeral socket) gets a stable hue so two flows to the
/// same node — and a node's pre-crash vs post-reboot sockets — read as visibly
/// different arrows even though they collapse onto one node lane. Index 0 is the
/// historic repl purple so single-socket diagrams look unchanged. Hues are
/// chosen legible against white and distinct from the SIP blue.
const CONN_PALETTE: &[&str] = &[
    "#9333ea", // purple
    "#0891b2", // cyan
    "#ca8a04", // amber
    "#16a34a", // green
    "#db2777", // pink
    "#7c3aed", // violet
    "#0d9488", // teal
    "#ea580c", // orange
];

/// Deterministically map a connection/socket tag (e.g. `:40007`) to a palette
/// INDEX. A plain byte-sum — NOT a hashing RNG — so the same socket gets the
/// same color across runs, processes, and the two renderers.
fn conn_palette_index(conn: &str) -> usize {
    conn.bytes().map(|b| b as usize).sum::<usize>() % CONN_PALETTE.len()
}

/// The palette color for a connection/socket tag.
fn conn_color(conn: &str) -> &'static str {
    CONN_PALETTE[conn_palette_index(conn)]
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
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

/// One anomaly resolved for rendering: the doc anomaly plus the diagram
/// ordinals (`data-idx`) of the rows its `row_seqs` link to. Views are in
/// DISPLAY order — gating findings first, original order within each severity —
/// and every rendered piece (the anomalies list, the per-row badges, the
/// detail-panel context) indexes into the same vector so they stay consistent.
struct AnomalyView<'a> {
    anomaly: &'a Anomaly,
    ords: Vec<usize>,
}

fn anomaly_views<'a>(doc: &'a SeqDoc, items: &[Item<'_>]) -> Vec<AnomalyView<'a>> {
    // Message rows only: a lifecycle band may BORROW a frame's seq (the chaos
    // overlay does) and is not clickable, so it never resolves a link; nor is a
    // view chip.
    let mut ord_of: HashMap<u64, usize> = HashMap::new();
    for (ord, item) in items.iter().enumerate() {
        if let Item::Row(row) = item {
            if matches!(row.kind, RowKind::Sip { .. } | RowKind::Repl { .. }) {
                ord_of.entry(row.seq).or_insert(ord);
            }
        }
    }
    let mut views: Vec<AnomalyView<'a>> = doc
        .anomalies
        .iter()
        .map(|a| AnomalyView {
            anomaly: a,
            ords: a.row_seqs.iter().filter_map(|s| ord_of.get(s).copied()).collect(),
        })
        .collect();
    // Gating first; the sort is stable, so recorded order survives within each
    // severity tier.
    views.sort_by_key(|v| !v.anomaly.is_gating());
    views
}

/// Row ordinal → indices (into the display-ordered views) of the anomalies
/// linked to that row.
fn row_anomaly_map(views: &[AnomalyView<'_>]) -> HashMap<usize, Vec<usize>> {
    let mut map: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, v) in views.iter().enumerate() {
        for &ord in &v.ords {
            map.entry(ord).or_default().push(i);
        }
    }
    map
}

/// The severity class shared by list items, badges, and detail chips. `None`
/// severity renders as advisory (pre-severity docs).
fn severity_class(a: &Anomaly) -> &'static str {
    if a.is_gating() {
        "gating"
    } else {
        "advisory"
    }
}

/// Render the whole [`SeqDoc`] as one HTML document string.
pub fn render_html(doc: &SeqDoc) -> String {
    let items = doc.sorted_items();
    let base = doc.base_ms();
    let lane_idx: std::collections::HashMap<&str, usize> =
        doc.lanes.iter().enumerate().map(|(i, l)| (l.id.as_str(), i)).collect();

    let views = anomaly_views(doc, &items);
    let anoms_of_row = row_anomaly_map(&views);
    let svg = svg_markup(doc, &items, base, &lane_idx, &anoms_of_row, &views);
    let payloads = render_payloads(doc, &items, base, &anoms_of_row, &views);
    let anomalies = render_anomalies(doc, &items, base, &views);
    let views_sections = render_views_sections(doc, base);

    let status = if doc.passed { "PASS" } else { "FAIL" };
    let status_color = if doc.passed { "#059669" } else { "#dc2626" };
    // Severity split next to the raw count, so the header says how bad, not
    // just how many.
    let gating_n = views.iter().filter(|v| v.anomaly.is_gating()).count();
    let severity_split = if doc.anomalies.is_empty() {
        String::new()
    } else {
        format!(
            " (<span style=\"color:{GATING_COLOR}\">{gating_n} gating</span> · \
             <span style=\"color:{ADVISORY_COLOR}\">{} advisory</span>)",
            doc.anomalies.len() - gating_n,
        )
    };
    let desc = doc
        .description
        .as_deref()
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!("<p class=\"desc\">{}</p>", escape(d)))
        .unwrap_or_default();

    // Per-flow color legend (036 ask C): SIP rows carrying a `conn` (the
    // Call-ID) are colored per flow — name each color so a reader can map
    // arrow hue → dialog without opening payloads. First-seen order.
    let mut flow_chips = String::new();
    let mut seen_conns: Vec<&str> = Vec::new();
    for item in &items {
        let Item::Row(row) = item else { continue };
        let (RowKind::Sip { .. }, Some(conn)) = (&row.kind, row.conn.as_deref()) else {
            continue;
        };
        if seen_conns.contains(&conn) {
            continue;
        }
        seen_conns.push(conn);
        let shown: String = if conn.chars().count() > 28 {
            format!("{}…", conn.chars().take(27).collect::<String>())
        } else {
            conn.to_string()
        };
        flow_chips.push_str(&format!(
            "<span><i class=\"swatch\" style=\"border-top-color:{}\"></i>Call-ID {}</span>\n",
            conn_color(conn),
            escape(&shown),
        ));
    }

    // When the doc is wall-clock-aligned, state the absolute anchor so every
    // relative `T+…` stamp maps to a real UTC instant (the "proper reference"
    // for correlating a callflow to external events like a chaos kill).
    let timeref = match doc.epoch_base_ms {
        Some(_) => format!(
            "<p class=\"desc\">Timeline t0 = <b>{}</b> (UTC) — all <span class=\"ts\">T+…</span> are relative to this; absolute UTC shown per message.</p>",
            crate::format_epoch_utc(doc.epoch_at(base).unwrap_or(base))
        ),
        None => String::new(),
    };

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
  /* Rows an anomaly links to, highlighted after clicking that anomaly. */
  .seq-msg.anomaly-hit rect {{ fill: rgba(220, 38, 38, 0.10); }}
  .seq-msg.anomaly-hit.selected rect {{ fill: rgba(220, 38, 38, 0.20); }}
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
  .anomalies ul {{ list-style: none; padding-left: 0; margin: 0.5rem 0 0; }}
  .anomaly {{ margin: 3px 0; padding: 4px 8px; border-left: 3px solid transparent;
             border-radius: 4px; font-size: 0.9rem; }}
  .anomaly.gating {{ border-left-color: {GATING_COLOR}; background: #fef2f2; }}
  .anomaly.advisory {{ border-left-color: {ADVISORY_COLOR}; background: #fffbeb; }}
  .anomaly.linked {{ cursor: pointer; }}
  .anomaly.linked:hover .jump {{ text-decoration: underline; }}
  .anomaly.selected {{ outline: 2px solid #2563eb; }}
  .sev {{ display: inline-block; font-size: 10px; font-weight: 700; padding: 1px 6px;
         border-radius: 8px; margin-right: 6px; vertical-align: 1px; }}
  .sev.gating {{ background: {GATING_COLOR}; color: #fff; }}
  .sev.advisory {{ background: #fbbf24; color: #451a03; }}
  /* The "→ <first linked message>" affordance on a linked anomaly. */
  .jump {{ color: #2563eb; font-family: monospace; font-size: 11px; margin-left: 6px;
          white-space: nowrap; }}
  /* Anomaly context shown with a message's payload in the detail panel. */
  /* Views plane: the belief table + the disagreements list under the diagram. */
  .views {{ padding: 12px 20px; border-top: 1px solid #e5e7eb; }}
  .views table {{ border-collapse: collapse; font-size: 0.85rem; }}
  .views th, .views td {{ border: 1px solid #e5e7eb; padding: 3px 8px; text-align: left;
                         white-space: nowrap; }}
  .views th {{ background: #f3f4f6; }}
  .views td.disputed {{ background: {DISPUTE_FILL}; }}
  .views td.moved {{ font-weight: 600; color: {VIEW_COLOR}; }}
  .views .sig {{ color: #6b7280; font-size: 0.75rem; }}
  .views ul {{ list-style: none; padding-left: 0; }}
  .views li {{ margin: 3px 0; padding: 4px 8px; border-left: 3px solid {VIEW_COLOR};
              background: {VIEW_FILL}; border-radius: 4px; font-size: 0.9rem; }}
  .payload-anoms .pa {{ border-left: 3px solid; padding: 4px 8px; margin: 6px 0;
                       border-radius: 4px; font-size: 12px; }}
  .pa.gating {{ border-color: {GATING_COLOR}; background: #fef2f2; }}
  .pa.advisory {{ border-color: {ADVISORY_COLOR}; background: #fffbeb; }}
</style></head>
<body>
  <header>
    <h1>Unified sequence: {title}</h1>
    <p>Status: <span class="status">{status}</span> &middot; {anomaly_count} anomalies recorded{severity_split}</p>
    {desc}
    {timeref}
    <div class="legend">
      <span><i class="swatch" style="border-top-color:{SIP_COLOR}"></i>SIP</span>
      <span><i class="swatch" style="border-top-color:{REPL_COLOR};border-top-style:dashed"></i>Replication (dashed; hue = per-socket connection)</span>
      <span><i class="swatch" style="border-top-color:{BAND_COLOR}"></i>Lifecycle (crash / reboot / failover / partition)</span>
      <span><i class="swatch" style="border-top-color:{VIEW_COLOR}"></i>View (what an observer believes about a node)</span>
      <span style="color:{LOST_COLOR}">✗ lost — frame emitted into a dead / superseded socket; the stub stops short of the lane (never reached the live node)</span>
      {flow_chips}
    </div>
  </header>
  <div class="main">
    <div class="diagram-panel">{svg}{anomalies}{views_sections}</div>
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
    function showRow(g) {{
      document.querySelectorAll('.seq-msg.selected').forEach(s => s.classList.remove('selected'));
      g.classList.add('selected');
      const src = document.getElementById('evt-' + g.dataset.idx);
      document.querySelector('.detail-body').innerHTML = src ? src.innerHTML
          : '<div class="detail-placeholder">No payload recorded for this message</div>';
    }}
    document.querySelectorAll('.seq-msg').forEach(g => g.addEventListener('click', () => showRow(g)));
    // A linked anomaly jumps to its offending message(s): highlight every
    // linked row, scroll the first into view, and open its payload (which
    // carries the anomaly context) in the detail panel.
    document.querySelectorAll('.anomaly.linked').forEach(li => li.addEventListener('click', () => {{
      document.querySelectorAll('.anomaly.selected').forEach(s => s.classList.remove('selected'));
      li.classList.add('selected');
      document.querySelectorAll('.seq-msg.anomaly-hit').forEach(s => s.classList.remove('anomaly-hit'));
      const ords = li.dataset.rows.split(',');
      ords.forEach(o => {{
        const g = document.querySelector('.seq-msg[data-idx="' + o + '"]');
        if (g) g.classList.add('anomaly-hit');
      }});
      const first = document.querySelector('.seq-msg[data-idx="' + ords[0] + '"]');
      if (first) {{
        first.scrollIntoView({{ block: 'center', inline: 'nearest' }});
        showRow(first);
      }}
    }}));
  </script>
</body></html>"#,
        title = escape(&doc.title),
        anomaly_count = doc.anomalies.len(),
    )
}

/// Render ONLY the SVG sequence diagram — the exact markup [`render_html`]
/// embeds in its diagram panel. For callers that persist/serve the diagram
/// standalone (the E2E `result.json` sibling artifacts, ADR-0018 Phase F).
pub fn render_svg(doc: &SeqDoc) -> String {
    let items = doc.sorted_items();
    let base = doc.base_ms();
    let lane_idx: std::collections::HashMap<&str, usize> =
        doc.lanes.iter().enumerate().map(|(i, l)| (l.id.as_str(), i)).collect();
    let views = anomaly_views(doc, &items);
    svg_markup(doc, &items, base, &lane_idx, &row_anomaly_map(&views), &views)
}

/// Render the diagram as a SELF-CONTAINED, EMBEDDABLE fragment for a host page
/// (the E2E cell page): the clickable SVG in a bounded scrollable pane on the
/// LEFT, a FIXED message-detail pane on the RIGHT, the hidden per-message
/// payload blocks, and the click `<script>` — everything scoped under
/// `.seq-embed` with its own `<style>` so it drops into a host document without
/// colliding with the host CSS. Unlike [`render_html`] it emits NO
/// `<html>/<head>/<body>` chrome; unlike [`render_svg`] the messages are wired
/// to actually reveal their payload on click. The script scopes its lookups to
/// the embed root, so multiple embeds (or other page content) never interfere.
pub fn render_embed(doc: &SeqDoc) -> String {
    let items = doc.sorted_items();
    let base = doc.base_ms();
    let lane_idx: std::collections::HashMap<&str, usize> =
        doc.lanes.iter().enumerate().map(|(i, l)| (l.id.as_str(), i)).collect();
    let views = anomaly_views(doc, &items);
    let anoms_of_row = row_anomaly_map(&views);
    let svg = svg_markup(doc, &items, base, &lane_idx, &anoms_of_row, &views);
    let payloads = render_payloads(doc, &items, base, &anoms_of_row, &views);

    format!(
        r#"<div class="seq-embed">
<style>
  .seq-embed {{ margin: .5rem 0; }}
  .seq-embed .seq-main {{ display: flex; height: 70vh; border: 1px solid #e5e7eb;
        border-radius: 6px; overflow: hidden; }}
  .seq-embed .seq-diagram {{ flex: 1; overflow: auto; padding: 16px; }}
  .seq-embed .seq-diagram svg {{ display: block; }}
  .seq-embed .seq-detail {{ width: 460px; border-left: 1px solid #e5e7eb; background: #fff;
        display: flex; flex-direction: column; overflow: hidden; }}
  .seq-embed .seq-detail-header {{ padding: 10px 14px; background: #f3f4f6;
        border-bottom: 1px solid #e5e7eb; font-size: 13px; font-weight: 600; color: #374151; }}
  .seq-embed .detail-body {{ flex: 1; overflow: auto; padding: 14px; }}
  .seq-embed .detail-placeholder {{ color: #9ca3af; font-style: italic; padding: 20px; text-align: center; }}
  .seq-embed .payload-head {{ margin-bottom: 8px; }}
  .seq-embed .seq-sip .payload-head {{ color: {SIP_COLOR}; }}
  .seq-embed .seq-repl .payload-head {{ color: {REPL_COLOR}; }}
  .seq-embed .ts {{ color: #6b7280; font-family: monospace; }}
  .seq-embed .seq-msg {{ cursor: pointer; }}
  .seq-embed .seq-msg:hover line {{ stroke-width: 3; }}
  .seq-embed .seq-msg:hover text {{ text-decoration: underline; }}
  .seq-embed .seq-msg:hover rect {{ fill: rgba(37, 99, 235, 0.05); }}
  .seq-embed .seq-msg.selected rect {{ fill: rgba(37, 99, 235, 0.12); }}
  .seq-embed .payload {{ display: none; }}
  .seq-embed .payload-anoms .pa {{ border-left: 3px solid; padding: 4px 8px; margin: 6px 0;
        border-radius: 4px; font-size: 12px; }}
  .seq-embed .pa.gating {{ border-color: {GATING_COLOR}; background: #fef2f2; }}
  .seq-embed .pa.advisory {{ border-color: {ADVISORY_COLOR}; background: #fffbeb; }}
  .seq-embed .detail-body pre {{ background: #f9fafb; padding: 8px; border-radius: 4px; margin: 4px 0;
        font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace;
        white-space: pre-wrap; overflow-wrap: anywhere; overflow: visible; max-height: none; }}
</style>
<div class="seq-main">
  <div class="seq-diagram">{svg}</div>
  <div class="seq-detail">
    <div class="seq-detail-header">Message detail</div>
    <div class="detail-body"><div class="detail-placeholder">Click a message to inspect</div></div>
  </div>
</div>
<!-- Hidden, already-HTML-escaped payload blocks, one per diagram message. -->
{payloads}
<script>
  (function() {{
    var root = document.currentScript.closest('.seq-embed');
    root.querySelectorAll('.seq-msg').forEach(function(g) {{
      g.addEventListener('click', function() {{
        root.querySelectorAll('.seq-msg.selected').forEach(function(s) {{ s.classList.remove('selected'); }});
        g.classList.add('selected');
        var src = root.querySelector('#evt-' + g.dataset.idx);
        root.querySelector('.detail-body').innerHTML = src ? src.innerHTML
            : '<div class="detail-placeholder">No payload recorded for this message</div>';
      }});
    }});
  }})();
</script>
</div>
"#
    )
}

/// The per-row timestamp label: relative `T+…`, plus the absolute UTC instant
/// when the doc is wall-clock-aligned (so a frame — or a chaos-marker band —
/// carries a real time that correlates to external events).
fn ts_label(doc: &SeqDoc, at_ms: i64, base: i64) -> String {
    let rel = format_relative(at_ms - base);
    match doc.epoch_at(at_ms) {
        Some(e) => format!("{rel} · {}", crate::format_epoch_utc(e)),
        None => rel,
    }
}

fn svg_markup(
    doc: &SeqDoc,
    items: &[Item<'_>],
    base: i64,
    lane_idx: &std::collections::HashMap<&str, usize>,
    anoms_of_row: &HashMap<usize, Vec<usize>>,
    views: &[AnomalyView<'_>],
) -> String {
    let n_lanes = doc.lanes.len().max(1);
    let width = LEFT_PAD + (n_lanes as i64) * LANE_GAP;
    let height = TOP_PAD + (items.len() as i64) * ROW_GAP + BOTTOM_PAD;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{width}\" height=\"{height}\" font-family=\"system-ui, sans-serif\" font-size=\"12\">\n"
    ));
    // Arrowhead markers: one for SIP, plus one per per-socket palette color so a
    // colored repl arrow gets a matching colored arrowhead (`ah-conn-{i}`).
    s.push_str("<defs>");
    s.push_str(&format!(
        "<marker id=\"ah-sip\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"3\" orient=\"auto\"><path d=\"M0,0 L7,3 L0,6 Z\" fill=\"{SIP_COLOR}\"/></marker>"
    ));
    for (i, c) in CONN_PALETTE.iter().enumerate() {
        s.push_str(&format!(
            "<marker id=\"ah-conn-{i}\" markerWidth=\"8\" markerHeight=\"8\" refX=\"7\" refY=\"3\" orient=\"auto\"><path d=\"M0,0 L7,3 L0,6 Z\" fill=\"{c}\"/></marker>"
        ));
    }
    s.push_str("</defs>\n");

    // Lifelines + column heads. Consecutive lanes sharing a `group` (logical
    // sub-lanes of one socket — 036 ask C) get one bracketing header with the
    // shared resource (the ip:port) centered above their individual captions.
    let life_bottom = height - BOTTOM_PAD / 2;
    {
        let mut i = 0;
        while i < doc.lanes.len() {
            let Some(group) = &doc.lanes[i].group else {
                i += 1;
                continue;
            };
            let mut j = i + 1;
            while j < doc.lanes.len() && doc.lanes[j].group.as_deref() == Some(group.as_str()) {
                j += 1;
            }
            let (x1, x2) = (lane_x(i), lane_x(j - 1));
            let y = TOP_PAD - 52;
            s.push_str(&format!(
                "<text x=\"{}\" y=\"{y}\" text-anchor=\"middle\" font-weight=\"bold\" fill=\"#6b7280\">{}</text>\n",
                (x1 + x2) / 2,
                escape(group),
            ));
            s.push_str(&format!(
                "<path d=\"M{} {} L{} {} L{} {} L{} {}\" fill=\"none\" stroke=\"#9ca3af\" stroke-width=\"1\"/>\n",
                x1 - 20, y + 10, x1 - 20, y + 16, x2 + 20, y + 16, x2 + 20, y + 10,
            ));
            i = j;
        }
    }
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
    for (ord, item) in items.iter().enumerate() {
        let y = TOP_PAD + (ord as i64) * ROW_GAP + ROW_GAP / 2;
        let ts = ts_label(doc, item.at_ms(), base);
        let row = match item {
            Item::View(v) => {
                view_chip(&mut s, v, y, lane_idx, &ts);
                continue;
            }
            Item::Row(row) => row,
        };
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
                let is_repl = matches!(row.kind, RowKind::Repl { .. });
                // Per-key color for arrows carrying a `conn`: repl rows key on
                // the socket (two flows to the same node, pre-crash vs
                // post-reboot sockets); SIP rows key on the Call-ID (036 ask C
                // — a b2bua's a-leg vs b-leg read as distinct flows). A row
                // with no `conn` falls back to its plane's default color.
                let color = match (&row.conn, is_repl) {
                    (Some(c), _) => conn_color(c),
                    (None, true) => REPL_COLOR,
                    (None, false) => SIP_COLOR,
                };
                let marker = match (&row.conn, is_repl) {
                    (Some(c), _) => format!("ah-conn-{}", conn_palette_index(c)),
                    (None, true) => "ah-conn-0".to_string(),
                    (None, false) => "ah-sip".to_string(),
                };
                let dash = if is_repl { " stroke-dasharray=\"5 3\"" } else { "" };
                let plane_class = if is_repl { "seq-repl" } else { "seq-sip" };
                // Anomaly badge: a row any finding links to carries a ⚠ at its
                // left end, colored by the WORST linked severity, so the eye
                // finds the offending messages without opening the list.
                let row_anoms = anoms_of_row.get(&ord);
                let badge_color = row_anoms.map(|idxs| {
                    if idxs.iter().any(|&i| views[i].anomaly.is_gating()) {
                        GATING_COLOR
                    } else {
                        ADVISORY_COLOR
                    }
                });
                // The socket tag rendered inline so distinct connections are
                // nameable, not just colored (e.g. `:40007` vs the live
                // `:40011`). Repl-only: a SIP row's `conn` is its Call-ID —
                // far too long inline; the legend names those colors instead.
                let sock = if is_repl {
                    row.conn.as_deref().map(|c| format!(" {c}")).unwrap_or_default()
                } else {
                    String::new()
                };

                let fi = lane_idx.get(row.from.as_str()).copied().unwrap_or(0);
                let ti = row.to.as_deref().and_then(|t| lane_idx.get(t).copied()).unwrap_or(fi);
                let (x1, x2) = (lane_x(fi), lane_x(ti));
                let opacity = if delivered { "1" } else { "0.5" };
                // Each message is a clickable `<g class="seq-msg" data-idx="{ord}">`
                // whose payload lives in the hidden `#evt-{ord}` block. A trailing
                // transparent full-row `<rect>` makes the whole row clickable.
                s.push_str(&format!(
                    "<g class=\"seq-msg {plane_class}\" data-idx=\"{ord}\" style=\"cursor:pointer\">\n"
                ));
                if x1 == x2 {
                    // Self-message: a small loop tag at the lane.
                    s.push_str(&format!(
                        "<text x=\"{}\" y=\"{}\" fill=\"{color}\" opacity=\"{opacity}\">{}{sock} {}</text>\n",
                        x1 + 6,
                        y,
                        escape(&row.label),
                        if delivered { "" } else { "✗" },
                    ));
                } else if delivered {
                    s.push_str(&format!(
                        "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{x2}\" y2=\"{y}\" stroke=\"{color}\" stroke-width=\"1.5\" opacity=\"{opacity}\" marker-end=\"url(#{marker})\"{dash}/>\n"
                    ));
                    let mid = (x1 + x2) / 2;
                    s.push_str(&format!(
                        "<text x=\"{mid}\" y=\"{}\" text-anchor=\"middle\" fill=\"{color}\" opacity=\"{opacity}\">{}{sock}</text>\n",
                        y - 4,
                        escape(&row.label),
                    ));
                } else {
                    // LOST: the frame was emitted into a dead / superseded socket
                    // and never arrived. Draw a stub that visibly STOPS SHORT of
                    // the target lane (no arrowhead touching it) and cap it with a
                    // red ✗ — so the eye sees it never reached the live node on
                    // that lane; the socket tag + its color name the dead conn.
                    let xstub = x1 + (x2 - x1) * 65 / 100;
                    s.push_str(&format!(
                        "<line x1=\"{x1}\" y1=\"{y}\" x2=\"{xstub}\" y2=\"{y}\" stroke=\"{color}\" stroke-width=\"1.5\" opacity=\"0.5\"{dash}/>\n"
                    ));
                    let mid = (x1 + xstub) / 2;
                    s.push_str(&format!(
                        "<text x=\"{mid}\" y=\"{}\" text-anchor=\"middle\" fill=\"{color}\" opacity=\"0.8\">{}{sock} ✗ lost</text>\n",
                        y - 4,
                        escape(&row.label),
                    ));
                    // The bold red ✗ at the severed end.
                    s.push_str(&format!(
                        "<text x=\"{xstub}\" y=\"{}\" text-anchor=\"middle\" fill=\"{LOST_COLOR}\" font-size=\"15\" font-weight=\"bold\">✗</text>\n",
                        y + 5,
                    ));
                }
                if let Some(color) = badge_color {
                    s.push_str(&format!(
                        "<text x=\"{}\" y=\"{}\" text-anchor=\"middle\" fill=\"{color}\" font-size=\"13\" font-weight=\"bold\">⚠</text>\n",
                        x1.min(x2) - 12,
                        y + 4,
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
fn render_payloads(
    doc: &SeqDoc,
    items: &[Item<'_>],
    base: i64,
    anoms_of_row: &HashMap<usize, Vec<usize>>,
    views: &[AnomalyView<'_>],
) -> String {
    let mut out = String::new();
    for (ord, item) in items.iter().enumerate() {
        let Item::Row(row) = item else { continue };
        let ts = ts_label(doc, row.at_ms, base);
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
                let to =
                    row.to.as_deref().map(|t| lane_caption(doc, t)).unwrap_or_else(|| "?".into());
                // A colored socket chip so the connection is identifiable in the
                // detail panel too (same hue as its arrow).
                let conn_chip = row
                    .conn
                    .as_deref()
                    .map(|c| {
                        format!(
                            " &nbsp; <code style=\"color:{}\">conn {}</code>",
                            conn_color(c),
                            escape(c)
                        )
                    })
                    .unwrap_or_default();
                let badge = match (delivered, row.conn.as_deref()) {
                    (true, _) => String::new(),
                    (false, Some(c)) => format!(" ✗ LOST IN TRANSIT (defunct conn {})", escape(c)),
                    (false, None) => " ✗ LOST IN TRANSIT".to_string(),
                };
                let body = match row.detail.as_deref().filter(|d| !d.trim().is_empty()) {
                    Some(d) => format!("<pre>{}</pre>", escape(d)),
                    None => "<div class=\"detail-placeholder\">No payload recorded for this message</div>".to_string(),
                };
                // Anomalies linked to this message, shown WITH the payload so a
                // clicked row states its findings without a list round-trip.
                let anoms = match anoms_of_row.get(&ord) {
                    None => String::new(),
                    Some(idxs) => {
                        let mut s = String::from("<div class=\"payload-anoms\">");
                        for &i in idxs {
                            let a = views[i].anomaly;
                            s.push_str(&format!(
                                "<div class=\"pa {}\">⚠ <code>{}</code> {}</div>",
                                severity_class(a),
                                escape(&a.check),
                                escape(&a.detail),
                            ));
                        }
                        s.push_str("</div>");
                        s
                    }
                };
                out.push_str(&format!(
                    "<div class=\"payload {class}\" id=\"evt-{ord}\" hidden><div class=\"payload-head\"><code>{from} → {to}</code>{conn_chip} &nbsp; <b>[{plane}] {}</b> &nbsp; <span class=\"ts\">{}</span>{badge}</div>{anoms}{body}</div>\n",
                    escape(&row.label),
                    escape(&ts),
                ));
            }
        }
    }
    out
}

/// The anomalies list, in display order (gating first). A finding whose
/// `row_seqs` resolved to diagram rows renders as a clickable `.linked` item
/// carrying `data-rows` (the ordinals) and a `→ <first linked message>` jump
/// affordance; the click handler highlights the rows and opens the first one.
fn render_anomalies(
    doc: &SeqDoc,
    items: &[Item<'_>],
    base: i64,
    views: &[AnomalyView<'_>],
) -> String {
    if views.is_empty() {
        return String::new();
    }
    let mut out = String::from("<div class=\"anomalies\"><h2>Anomalies</h2>\n<ul>\n");
    for v in views {
        let a = v.anomaly;
        let lane = a.lane.as_deref().map(|l| format!(" [{}]", escape(l))).unwrap_or_default();
        let sev = severity_class(a);
        let (link_class, data_rows, jump) = if v.ords.is_empty() {
            (String::new(), String::new(), String::new())
        } else {
            let first = &items[v.ords[0]];
            let more = match v.ords.len() {
                1 => String::new(),
                n => format!(" (+{} more)", n - 1),
            };
            (
                " linked".to_string(),
                format!(
                    " data-rows=\"{}\"",
                    v.ords.iter().map(|o| o.to_string()).collect::<Vec<_>>().join(","),
                ),
                format!(
                    "<span class=\"jump\">→ {} @ {}{more}</span>",
                    escape(&first.label()),
                    escape(&ts_label(doc, first.at_ms(), base)),
                ),
            )
        };
        out.push_str(&format!(
            "<li class=\"anomaly {sev}{link_class}\"{data_rows}><span class=\"sev {sev}\">{}</span><code>{}</code>{lane}: {} {jump}</li>\n",
            if a.is_gating() { "GATING" } else { "advisory" },
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

/// A belief change on the timeline: a small tinted chip anchored on the
/// OBSERVER's own column (never a full-width band — a view is one actor's, not
/// the cluster's). Not clickable: it carries no payload.
fn view_chip(
    s: &mut String,
    v: &ViewChange,
    y: i64,
    lane_idx: &std::collections::HashMap<&str, usize>,
    ts: &str,
) {
    let x = lane_x(lane_idx.get(v.lane.as_str()).copied().unwrap_or(0));
    let text = format!("{}: {} = {} ({})", v.observer, v.subject, v.belief, v.signal);
    let w = 8 + 6 * text.chars().count() as i64;
    s.push_str(&format!(
        "<rect x=\"{}\" y=\"{}\" width=\"{w}\" height=\"18\" rx=\"4\" fill=\"{VIEW_FILL}\" stroke=\"{VIEW_COLOR}\" stroke-width=\"0.8\"/>\n",
        x - 4,
        y - 13,
    ));
    s.push_str(&format!(
        "<text x=\"{}\" y=\"{}\" fill=\"{VIEW_COLOR}\" font-size=\"11\">{}</text>\n",
        x + 2,
        y,
        escape(&text),
    ));
    s.push_str(&format!(
        "<text x=\"6\" y=\"{}\" fill=\"#6b7280\" font-family=\"monospace\" font-size=\"10\">{}</text>\n",
        y + 3,
        escape(ts),
    ));
}

/// The views table (one row per change instant, one column per observer, cells
/// tinted where the observers disagree) and the disagreements list under it.
/// Empty string when the doc records no beliefs.
fn render_views_sections(doc: &SeqDoc, base: i64) -> String {
    if doc.views.is_empty() {
        return String::new();
    }
    let table = views_table(&doc.views);
    let mut out = String::from(
        "<div class=\"views\"><h2>Views — what each observer believed</h2>\n<table>\n<tr><th>time</th><th>subject</th>",
    );
    for o in &table.observers {
        out.push_str(&format!("<th>{}</th>", escape(o)));
    }
    out.push_str("</tr>\n");
    for row in &table.rows {
        out.push_str(&format!(
            "<tr><td><span class=\"ts\">{}</span></td><td><code>{}</code></td>",
            escape(&format_relative(row.at_ms - base)),
            escape(&row.subject),
        ));
        for cell in &row.cells {
            let class = match (row.disputed, cell.as_ref().is_some_and(|c| c.changed)) {
                (true, true) => " class=\"disputed moved\"",
                (true, false) => " class=\"disputed\"",
                (false, true) => " class=\"moved\"",
                (false, false) => "",
            };
            match cell {
                Some(c) => out.push_str(&format!(
                    "<td{class}>{} <span class=\"sig\">{}</span></td>",
                    escape(&c.belief),
                    escape(&c.signal),
                )),
                None => out.push_str(&format!("<td{class}>—</td>")),
            }
        }
        out.push_str("</tr>\n");
    }
    out.push_str("</table>\n");

    let conflicts = disagreements(&doc.views);
    out.push_str(&format!("<h2>Disagreements ({})</h2>\n<ul>\n", conflicts.len()));
    if conflicts.is_empty() {
        out.push_str("<li>every observer agreed about every subject</li>\n");
    }
    for d in &conflicts {
        let until = match d.to_ms {
            Some(t) => format_relative(t - base),
            None => "end of run".to_string(),
        };
        let holders = d
            .holders
            .iter()
            .map(|(observer, belief, signal)| {
                format!(
                    "<b>{}</b> believes {} <span class=\"sig\">({})</span>",
                    escape(observer),
                    escape(belief),
                    escape(signal),
                )
            })
            .collect::<Vec<_>>()
            .join(" · ");
        out.push_str(&format!(
            "<li><span class=\"ts\">{} .. {}</span> about <code>{}</code>: {holders}</li>\n",
            escape(&format_relative(d.from_ms - base)),
            escape(&until),
            escape(&d.subject),
        ));
    }
    out.push_str("</ul></div>\n");
    out
}
