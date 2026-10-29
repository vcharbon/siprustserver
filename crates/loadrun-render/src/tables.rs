//! The bottom of the page: the latency, checkpoint, round-trip and check
//! tables, the rate events and the run's settings.

use std::fmt::Write as _;

use e2e_model::LoadRunIndex;

use crate::html::{clock, cps, esc, fmt_epoch_ms};
use crate::series::event_label;

pub(crate) fn latency(out: &mut String, idx: &LoadRunIndex) {
    out.push_str(
        "<h2>Latency (ms)</h2><table><tr><th>scenario</th><th>n</th><th>mean</th><th>p50</th>\
         <th>p90</th><th>p99</th><th>max</th></tr>",
    );
    for r in &idx.latency {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td>\
             <td>{:.1}</td></tr>",
            esc(&r.scenario),
            r.n,
            r.mean_ms,
            r.p50_ms,
            r.p90_ms,
            r.p99_ms,
            r.max_ms
        );
    }
    out.push_str("</table>");
}

pub(crate) fn checkpoints(out: &mut String, idx: &LoadRunIndex) {
    if idx.checkpoints.is_empty() {
        return;
    }
    out.push_str(
        "<h2>Checkpoints (ms)</h2><table><tr><th>scenario</th><th>checkpoint</th><th>n</th>\
         <th>p50</th><th>p90</th><th>p99</th></tr>",
    );
    for r in &idx.checkpoints {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td></tr>",
            esc(&r.scenario),
            esc(&r.checkpoint),
            r.n,
            r.p50_ms,
            r.p90_ms,
            r.p99_ms
        );
    }
    out.push_str("</table>");
}

pub(crate) fn rtt(out: &mut String, idx: &LoadRunIndex) {
    if idx.rtt.is_empty() {
        return;
    }
    out.push_str(
        "<h2>SIP round trips (ms)</h2><table><tr><th>scenario</th><th>exchange</th><th>n</th>\
         <th>p50</th><th>p90</th><th>p99</th></tr>",
    );
    for r in &idx.rtt {
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td></tr>",
            esc(&r.scenario),
            esc(&r.exchange),
            r.n,
            r.p50_ms,
            r.p90_ms,
            r.p99_ms
        );
    }
    out.push_str("</table>");
}

pub(crate) fn checks(out: &mut String, idx: &LoadRunIndex) {
    if idx.checks.is_empty() {
        return;
    }
    out.push_str(
        "<h2>Test-case checks (recorded calls)</h2><table><tr><th>scenario</th><th>passed</th>\
         <th>failed</th></tr>",
    );
    for r in &idx.checks {
        let cls = if r.failed == 0 { "ok" } else { "nok" };
        let _ = write!(
            out,
            "<tr><td>{}</td><td class=\"ok\">{}</td><td class=\"{cls}\">{}</td></tr>",
            esc(&r.scenario),
            r.passed,
            r.failed
        );
    }
    out.push_str("</table>");
}

pub(crate) fn rate_events(out: &mut String, idx: &LoadRunIndex) {
    out.push_str("<h2>Rate events</h2>");
    if idx.rate_events.is_empty() {
        out.push_str("<p class=\"muted\">no rate change</p>");
        return;
    }
    out.push_str("<table><tr><th>at</th><th>event</th></tr>");
    for e in &idx.rate_events {
        let _ = write!(out, "<tr><td>{}</td><td>{}</td></tr>", clock(e.at_ms), event_label(e));
    }
    out.push_str("</table>");
    if idx.rate_events_dropped > 0 {
        let _ = write!(out, "<p>+{} more</p>", idx.rate_events_dropped);
    }
}

pub(crate) fn settings(out: &mut String, idx: &LoadRunIndex) {
    let m = &idx.meta;
    let finished = if m.finished {
        format!("finished {}", fmt_epoch_ms(m.finished_ms))
    } else {
        format!("as of {}", fmt_epoch_ms(m.finished_ms))
    };
    let _ = write!(
        out,
        "<h2>Run</h2><table><tr><th>started</th><td>{}</td></tr><tr><th>state</th><td>{finished}</td></tr>\
         <tr><th>target</th><td><code>{}</code></td></tr><tr><th>configured rate</th><td>{} cps</td></tr>\
         <tr><th>duration</th><td>{} s</td></tr><tr><th>max in flight</th><td>{}</td></tr>",
        fmt_epoch_ms(m.started_ms),
        esc(&m.target),
        cps(m.cps),
        m.duration_secs,
        m.max_in_flight
    );
    if let Some(egress) = &m.egress {
        let _ = write!(out, "<tr><th>egress</th><td><code>{}</code></td></tr>", esc(egress));
    }
    if let Some(profile) = &m.profile {
        let _ = write!(out, "<tr><th>profile</th><td>{}</td></tr>", esc(profile));
    }
    if let Some(s) = &m.sampling {
        let _ = write!(
            out,
            "<tr><th>sampling</th><td>first {} per scenario, 1 in {}, {} after each new failure \
             mode; {} samples per bucket</td></tr>",
            s.sample_cap, s.record_every, s.chase_calls, s.sample_cap
        );
    }
    out.push_str("</table>");
}
