//! The time series: one sparkline per series over the whole run, then the
//! last windows newest first, each marked with the rate events it holds.

use std::fmt::Write as _;

use e2e_model::{LoadRunIndex, RateEvent, RateEventKind, SeriesPoint};

use crate::html::{clock, cps, esc};

/// The windows the table lists, newest first.
pub(crate) const TABLE_WINDOWS: usize = 20;

const SPARK_W: f64 = 240.0;
const SPARK_H: f64 = 28.0;

pub(crate) fn series(out: &mut String, idx: &LoadRunIndex) {
    out.push_str("<h2>Time series</h2>");
    let points = &idx.series;
    if points.is_empty() {
        out.push_str("<p class=\"muted\">no window closed yet</p>");
        return;
    }
    let span = points.last().map_or(0, SeriesPoint::end_ms);
    type Pick = fn(&SeriesPoint) -> f64;
    let lines: [(&str, Pick); 7] = [
        ("placed", |p| p.placed as f64),
        ("ok", |p| p.ok as f64),
        ("failed", |p| p.failed as f64),
        ("rejected", |p| p.rejected as f64),
        ("error %", |p| p.error_pct().unwrap_or(0.0)),
        ("p50 ms", |p| p.p50_ms),
        ("p99 ms", |p| p.p99_ms),
    ];
    out.push_str("<div>");
    for (label, pick) in lines {
        let values: Vec<f64> = points.iter().map(pick).collect();
        sparkline(out, label, points, &values, &idx.rate_events, span);
    }
    out.push_str("</div>");
    let _ = write!(
        out,
        "<p class=\"muted\">{} window(s) over {}; the last {TABLE_WINDOWS}, newest first. \
         Error % is the failed share of the calls that ended in the window.</p>",
        points.len(),
        clock(span)
    );
    out.push_str(
        "<table><tr><th>window</th><th>placed</th><th>ok</th><th>failed</th><th>rejected</th>\
         <th>error %</th><th>p50 ms</th><th>p99 ms</th><th>events</th></tr>",
    );
    for p in points.iter().rev().take(TABLE_WINDOWS) {
        let err = p.error_pct().map_or("–".to_string(), |e| format!("{e:.1}"));
        let marks = idx
            .rate_events
            .iter()
            .filter(|e| p.holds(e.at_ms))
            .map(event_label)
            .collect::<Vec<_>>()
            .join(", ");
        let failed = if p.failed == 0 {
            "0".to_string()
        } else {
            format!("<span class=\"nok\">{}</span>", p.failed)
        };
        let _ = write!(
            out,
            "<tr><td>{}–{}</td><td>{}</td><td>{}</td><td>{failed}</td><td>{}</td><td>{err}</td>\
             <td>{:.1}</td><td>{:.1}</td><td>{}</td></tr>",
            clock(p.start_ms),
            clock(p.end_ms()),
            p.placed,
            p.ok,
            p.rejected,
            p.p50_ms,
            p.p99_ms,
            esc(&marks)
        );
    }
    out.push_str("</table>");
}

/// One rate event as a short label (`rate 20 → 10`, `drain`).
pub(crate) fn event_label(e: &RateEvent) -> String {
    match e.kind {
        RateEventKind::Rate => format!("rate {} → {}", cps(e.from_cps), cps(e.to_cps)),
        RateEventKind::Drain => "drain".to_string(),
    }
}

/// One series as an inline SVG polyline over the run's whole span, scaled to
/// its own maximum, with a tick at each rate event.
fn sparkline(
    out: &mut String,
    label: &str,
    points: &[SeriesPoint],
    values: &[f64],
    events: &[RateEvent],
    span_ms: u64,
) {
    let max = values.iter().copied().fold(0.0_f64, f64::max);
    let x = |ms: u64| if span_ms == 0 { 0.0 } else { ms as f64 * SPARK_W / span_ms as f64 };
    let y =
        |v: f64| if max > 0.0 { SPARK_H - v * (SPARK_H - 2.0) / max - 1.0 } else { SPARK_H - 1.0 };
    let coords = points
        .iter()
        .zip(values)
        .map(|(p, v)| format!("{:.1},{:.1}", x(p.start_ms + p.span_ms / 2), y(*v)))
        .collect::<Vec<_>>()
        .join(" ");
    let _ = write!(
        out,
        "<span class=\"spark\">{label} <svg width=\"{SPARK_W}\" height=\"{SPARK_H}\" \
         viewBox=\"0 0 {SPARK_W} {SPARK_H}\"><title>{label}: max {max:.1}</title>"
    );
    for e in events {
        let ex = x(e.at_ms);
        let _ = write!(
            out,
            "<line x1=\"{ex:.1}\" y1=\"0\" x2=\"{ex:.1}\" y2=\"{SPARK_H}\" stroke=\"#c90\"/>"
        );
    }
    let _ = write!(
        out,
        "<polyline fill=\"none\" stroke=\"#336\" points=\"{coords}\"/></svg> max {max:.1}</span>"
    );
}
