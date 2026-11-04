//! The failures: the last failed calls, then every call counted by
//! scenario × class × case with its judged share, its error and its samples.

use std::fmt::Write as _;

use e2e_model::{CountRow, JudgedBasis, JudgedShare, LoadRunIndex, Sampling};

use crate::html::{clock, cut, esc};

/// The characters of an error detail the results table shows; the full text
/// is the cell's title.
pub(crate) const ERROR_CHARS: usize = 200;

/// The last failed calls, newest first.
pub(crate) fn failures(out: &mut String, idx: &LoadRunIndex, prefix: &str) {
    out.push_str("<h2>Last failures</h2>");
    if idx.recent_failures.is_empty() {
        out.push_str("<p class=\"muted\">no failed call</p>");
        return;
    }
    out.push_str(
        "<table><tr><th>at</th><th>scenario</th><th>class</th><th>case</th><th>sample</th></tr>",
    );
    for f in idx.recent_failures.iter().rev() {
        let sample = match &f.page {
            Some(page) => format!("<a href=\"{}{}\">callflow</a>", esc(prefix), esc(page)),
            None => "–".to_string(),
        };
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td class=\"nok\">{}</td><td>{}</td><td>{sample}</td></tr>",
            clock(f.at_ms),
            esc(&f.scenario),
            esc(&f.class),
            esc(&f.case)
        );
    }
    out.push_str("</table>");
}

/// The results table, under one line per scenario on the calls recorded.
pub(crate) fn results(out: &mut String, idx: &LoadRunIndex, prefix: &str) {
    out.push_str(
        "<h2>Results by scenario × class × case</h2>\
         <p>The <b>case</b> column splits a class into its distinct failure modes (RFC rule id, \
         failed check, failed step); each keeps its own first-N samples. The RFC audit's \
         trace rules and the case checks judge a call's recording: <b>judged share</b> is a \
         failure's share of the recorded calls they judged, and its estimate over the calls \
         that ended on an accepted branch.</p>",
    );
    for line in recording_lines(idx) {
        let _ = write!(out, "<p>{line}</p>");
    }
    out.push_str(
        "<table><tr><th>scenario</th><th>class</th><th>case</th><th>count</th>\
         <th>judged share</th><th>error</th><th>samples</th></tr>",
    );
    let none = none_recorded(idx.meta.sampling.as_ref());
    for row in &idx.counts {
        let cls = if row.ok { "ok" } else { "nok" };
        let judged = row.judged.as_ref().map(judged_text).unwrap_or_default();
        let _ = write!(
            out,
            "<tr><td>{}</td><td class=\"{cls}\">{}</td><td>{}</td><td>{}</td><td>{judged}</td>\
             <td>{}</td><td>{}</td></tr>",
            esc(&row.scenario),
            esc(&row.class),
            esc(&row.case),
            row.count,
            error_cell(row),
            sample_links(idx, row, prefix, &none)
        );
    }
    out.push_str("</table>");
}

/// The error cell: the first stored sample's detail, cut, with the full text
/// as its title; "no trace" for a failure bucket with none.
fn error_cell(row: &CountRow) -> String {
    match (&row.error, row.ok) {
        (Some(detail), _) => {
            format!("<span title=\"{}\">{}</span>", esc(detail), esc(&cut(detail, ERROR_CHARS)))
        }
        (None, false) => "<span class=\"muted\">no trace</span>".to_string(),
        (None, true) => String::new(),
    }
}

fn sample_links(idx: &LoadRunIndex, row: &CountRow, prefix: &str, none: &str) -> String {
    let group = idx
        .samples
        .iter()
        .find(|g| g.scenario == row.scenario && g.class == row.class && g.case == row.case);
    match group {
        Some(g) if !g.pages.is_empty() => g
            .pages
            .iter()
            .enumerate()
            .map(|(i, page)| format!("<a href=\"{}{}\">{i}</a>", esc(prefix), esc(page)))
            .collect::<Vec<_>>()
            .join(" "),
        _ => none.to_string(),
    }
}

/// What a bucket with no stored sample says: nothing when no sample is
/// stored at all, else how often a call records.
fn none_recorded(sampling: Option<&Sampling>) -> String {
    match sampling {
        None | Some(Sampling { sample_cap: 0, .. }) => String::new(),
        Some(Sampling { record_every: 0, .. }) => "none recorded".to_string(),
        Some(Sampling { record_every, .. }) => {
            format!("none recorded (1 in {record_every} calls is recorded)")
        }
    }
}

/// `7 of 150 audited (4.7 %), ≈ 56 of 1180`.
pub(crate) fn judged_text(j: &JudgedShare) -> String {
    let noun = match j.basis {
        JudgedBasis::Audited => "audited",
        JudgedBasis::Checked => "checked",
    };
    format!(
        "{} of {} {noun} ({:.1} %), ≈ {} of {}",
        j.failed,
        j.judged,
        j.share() * 100.0,
        j.estimate(),
        j.base
    )
}

/// One line per scenario: the calls placed and recorded, the calls the RFC
/// audit judged and those whose recording it judged, and the calls the case
/// checks judged.
fn recording_lines(idx: &LoadRunIndex) -> Vec<String> {
    let gates = match &idx.meta.sampling {
        Some(s) => {
            let mut gates = Vec::new();
            if s.sample_cap > 0 {
                gates.push(format!("the first {}", s.sample_cap));
            }
            if s.record_every > 0 {
                gates.push(format!("1 in {}", s.record_every));
            }
            format!(" ({})", gates.join(", "))
        }
        None => String::new(),
    };
    idx.recording
        .iter()
        .map(|r| {
            let mut line = format!(
                "<b>{}</b>: {} calls placed, {} recorded{gates}. {} ended on an accepted branch; \
                 the RFC audit judged the recording of {} of them",
                esc(&r.scenario),
                r.placed,
                r.recorded,
                r.accepted,
                r.audited
            );
            if let Some(c) = idx.checks.iter().find(|c| c.scenario == r.scenario) {
                let _ = write!(line, "; the case checks judged {}", c.passed + c.failed);
            }
            line.push('.');
            line
        })
        .collect()
}
