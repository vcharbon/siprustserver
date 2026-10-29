//! The top of the page: the command line the run was started with, then the
//! headline figures and, on a live page, the controls.

use std::fmt::Write as _;

use e2e_model::LoadRunIndex;

use crate::html::{clock, cps, esc, fmt_epoch_ms, shell_word};
use crate::Live;

/// The command line, its start time and its target.
pub(crate) fn command(out: &mut String, idx: &LoadRunIndex) {
    let m = &idx.meta;
    let line = m.command_line.iter().map(|a| shell_word(a)).collect::<Vec<_>>().join(" ");
    let line = if line.is_empty() { "(command line not recorded)".to_string() } else { line };
    let _ = write!(
        out,
        "<pre class=\"cmd\">{}</pre><p>started {} · target <code>{}</code></p>",
        esc(&line),
        fmt_epoch_ms(m.started_ms),
        esc(&m.target)
    );
}

/// The headline panel: where the run stands, and the controls when live.
pub(crate) fn headline(out: &mut String, idx: &LoadRunIndex, live: Option<Live>) {
    let (m, p, c) = (&idx.meta, &idx.progress, &idx.canaries);
    let state = if m.finished {
        "finished"
    } else if p.draining {
        "draining"
    } else {
        "running"
    };
    let rows = [
        (
            "elapsed",
            format!(
                "{} / {} ({state})",
                clock(p.elapsed_ms),
                clock(m.duration_secs.saturating_mul(1000))
            ),
        ),
        (
            "rate",
            format!(
                "target {} cps (configured {}) · achieved {:.1} cps",
                cps(p.target_cps),
                cps(m.cps),
                p.achieved_cps()
            ),
        ),
        ("in flight", format!("{} / {} max", p.in_flight, m.max_in_flight)),
        (
            "calls",
            format!(
                "offered {} · placed {} · <span class=\"ok\">ok {}</span> · {}",
                p.offered,
                p.placed,
                p.ok,
                flagged("failed", p.failed)
            ),
        ),
        (
            "rejected",
            format!(
                "{} · shed at max-in-flight {} · {}",
                flagged("rejected class", p.rejected),
                c.shed,
                flagged("generator failures", p.generator_failures)
            ),
        ),
        ("injected drops", format!("in {} · out {}", c.drops_in, c.drops_out)),
        (
            "retransmissions",
            if p.retransmit_engine {
                format!(
                    "received {} · re-sent {}",
                    p.retransmissions_received, p.retransmissions_resent
                )
            } else {
                format!(
                    "received {} · re-sent {} (retransmit engine off)",
                    p.retransmissions_received, p.retransmissions_resent
                )
            },
        ),
        (
            "mux",
            format!(
                "{} · {}",
                flagged("orphans", c.orphans),
                flagged("inbox drops", c.inbox_drops)
            ),
        ),
        (
            "18x delivery",
            format!(
                "{} ({} / {})",
                if c.ringing_ratio() >= 0.99 {
                    format!("<span class=\"ok\">{:.4}</span>", c.ringing_ratio())
                } else {
                    format!("<span class=\"nok\">{:.4}</span>", c.ringing_ratio())
                },
                c.ringing_received,
                c.ringing_expected
            ),
        ),
    ];
    out.push_str("<h2>Headline</h2><table>");
    for (label, value) in rows {
        let _ = write!(out, "<tr><th>{label}</th><td>{value}</td></tr>");
    }
    out.push_str("</table>");
    if let Some(live) = live {
        controls(out, idx, live);
    }
}

/// `label n`, red when `n` is not zero.
fn flagged(label: &str, n: u64) -> String {
    if n == 0 {
        format!("{label} 0")
    } else {
        format!("<span class=\"nok\">{label} {n}</span>")
    }
}

/// The rate and drain forms (posting to the load generator's `rate` and
/// `drain`, relative to the page, with the page's refresh period) and the
/// refresh links.
fn controls(out: &mut String, idx: &LoadRunIndex, live: Live) {
    let (p, n) = (&idx.progress, live.refresh_secs);
    let _ = write!(
        out,
        "<div class=\"controls\"><form class=\"inline\" method=\"post\" action=\"rate?refresh={n}\">\
         <input name=\"cps\" type=\"number\" min=\"0\" step=\"any\" value=\"{}\"> \
         <button type=\"submit\">set rate (cps)</button></form>",
        cps(p.target_cps)
    );
    if !p.draining {
        let _ = write!(
            out,
            "<form class=\"inline\" method=\"post\" action=\"drain?refresh={n}\">\
             <button type=\"submit\">drain</button></form>"
        );
    }
    if live.refresh_secs > 0 {
        let _ = write!(
            out,
            "<a href=\"?refresh={n}\">refresh</a> · reloads every {n} s · \
             <a href=\"?refresh=0\">freeze</a></div>",
            n = live.refresh_secs
        );
    } else {
        let _ = write!(
            out,
            "<a href=\"?refresh=0\">refresh</a> · frozen · \
             <a href=\"?refresh={}\">reload automatically</a></div>",
            crate::DEFAULT_REFRESH_SECS
        );
    }
}
