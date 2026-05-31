//! Self-contained HTML report — a minimal port of `html-report.ts`. Embeds the
//! SVG sequence diagram and lists every exchange with its full wire text in a
//! `<details>` block. No JavaScript: the click-to-inspect interactivity of the
//! source is reduced to native `<details>` panels so the artifact stays a
//! single static file with zero dependencies.

use layer_harness::RecordedScenario;
use sip_net::RecordedSipEntry;

use super::svg;
use super::wire::{facets, format_relative, wire_text};

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Render the whole report as one HTML document string.
pub fn render(
    scenario_name: &str,
    description: Option<&str>,
    entries: &[RecordedSipEntry],
    scenario: &RecordedScenario,
    passed: bool,
) -> String {
    let diagram = svg::render(entries, &scenario.lanes, scenario.transport_kind);
    let base_ts = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);
    let status = if passed { "PASS" } else { "FAIL" };
    let status_color = if passed { "#059669" } else { "#dc2626" };

    let mut rows = String::new();
    for entry in entries {
        let f = facets(&entry.raw);
        let rcvd = entry.received_ms.unwrap_or(entry.sent_ms) as i64 - base_ts;
        let badge = if entry.delivered { "" } else { " ⚠ UNDELIVERED" };
        rows.push_str(&format!(
            "<details><summary><code>{} → {}</code> &nbsp; <b>{}</b> &nbsp; <span class=\"ts\">{}</span>{}</summary><pre>{}</pre></details>\n",
            entry.from,
            entry.to,
            escape_html(&f.label),
            format_relative(rcvd),
            badge,
            escape_html(&wire_text(&entry.raw)),
        ));
    }

    let desc = description
        .map(|d| format!("<p class=\"desc\">{}</p>", escape_html(d)))
        .unwrap_or_default();

    let anomaly_count = scenario.anomalies.len();

    format!(
        r#"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<title>SIP Exchange Report — {name}</title>
<style>
  body {{ font-family: system-ui, sans-serif; margin: 2rem; color: #111827; }}
  h1 {{ font-size: 1.25rem; }}
  .status {{ font-weight: bold; color: {status_color}; }}
  .desc {{ color: #4b5563; }}
  .diagram {{ overflow-x: auto; border: 1px solid #e5e7eb; border-radius: 6px; padding: 8px; }}
  details {{ border-bottom: 1px solid #e5e7eb; padding: 4px 0; }}
  summary {{ cursor: pointer; }}
  .ts {{ color: #6b7280; font-family: monospace; }}
  pre {{ background: #f9fafb; padding: 8px; overflow-x: auto; border-radius: 4px; }}
</style></head>
<body>
  <h1>SIP Exchange Report: {name}</h1>
  <p>Status: <span class="status">{status}</span> &middot; {anomaly_count} anomalies recorded</p>
  {desc}
  <div class="diagram">{diagram}</div>
  <h2>Exchanges</h2>
  {rows}
</body></html>"#,
        name = escape_html(scenario_name),
    )
}
