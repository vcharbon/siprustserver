//! The message ladders: one per scenario, drawn through `seq-report` (each
//! arrow carrying its counts, its click panel the full counters, a failure
//! detected at it as a finding on it), then the same arrows as a table and
//! the failures that have no arrow.

use std::fmt::Write as _;

use e2e_model::{ClassCount, Ladder, LadderArrow, LoadRunIndex};
use seq_report::{Anomaly, Lane, LaneKind, RowKind, SeqDoc, SeqRow};

use crate::html::esc;

/// The SUT's lane name in a ladder.
const SUT: &str = "SUT";

pub(crate) fn ladders(out: &mut String, idx: &LoadRunIndex) {
    if idx.ladders.is_empty() {
        return;
    }
    out.push_str(
        "<h2>Message ladders</h2><p class=\"muted\">One per scenario: every message its calls \
         exchanged with the SUT, merged over the calls by sender, receiver, message and \
         occurrence, ordered by its mean time from call start. <b>retr</b>: copies received \
         after the first; <b>resent</b>: copies sent after the first; <b>errors</b>: failed \
         calls whose failure was detected at that message (the last one in before the \
         outcome).</p>",
    );
    for ladder in &idx.ladders {
        let engine = if ladder.engine { "" } else { " · retransmit engine off" };
        let _ = write!(out, "<h3>{} ({} calls{engine})</h3>", esc(&ladder.scenario), ladder.calls);
        out.push_str(&seq_report::render_embed(&seq_doc(ladder)));
        table(out, ladder);
    }
}

/// `INVITE`, `200 (INVITE) #2` for a second occurrence.
fn name(a: &LadderArrow) -> String {
    if a.nth > 1 {
        format!("{} #{}", a.label, a.nth)
    } else {
        a.label.clone()
    }
}

/// `timeout 4 · status_486 1`.
fn classes(c: &[ClassCount]) -> String {
    c.iter().map(|c| format!("{} {}", c.class, c.count)).collect::<Vec<_>>().join(" · ")
}

/// The arrow's caption: `INVITE ×120 · retr 3 · resent 2 · timeout 4`.
fn caption(a: &LadderArrow) -> String {
    let mut s = format!("{} ×{}", name(a), a.count);
    if a.retransmissions > 0 {
        let _ = write!(s, " · retr {}", a.retransmissions);
    }
    if a.resent > 0 {
        let _ = write!(s, " · resent {}", a.resent);
    }
    if !a.errors.is_empty() {
        let _ = write!(s, " · {}", classes(&a.errors));
    }
    s
}

fn seq_doc(l: &Ladder) -> SeqDoc {
    let lanes = l
        .lanes
        .iter()
        .map(|lane| {
            let kind = if lane == SUT { LaneKind::Sut } else { LaneKind::Ua };
            Lane::new(lane.clone(), lane.clone(), kind)
        })
        .collect();
    let rows = l
        .arrows
        .iter()
        .enumerate()
        .map(|(i, a)| SeqRow {
            at_ms: a.mean_offset_ms.round() as i64,
            seq: i as u64,
            from: a.from.clone(),
            to: Some(a.to.clone()),
            label: caption(a),
            detail: Some(format!(
                "{} {} → {}\ncalls: {}\nretransmissions received: {}\nre-sent: {}\n\
                 mean offset from call start: {:.1} ms\nerrors: {}",
                name(a),
                a.from,
                a.to,
                a.count,
                a.retransmissions,
                a.resent,
                a.mean_offset_ms,
                if a.errors.is_empty() { "none".to_string() } else { classes(&a.errors) }
            )),
            conn: None,
            kind: RowKind::Sip { delivered: true },
        })
        .collect();
    let anomalies = l
        .arrows
        .iter()
        .enumerate()
        .flat_map(|(i, a)| {
            a.errors.iter().map(move |e| Anomaly {
                check: e.class.clone(),
                detail: format!("{} call(s) failed at {}", e.count, name(a)),
                lane: Some(a.to.clone()),
                endpoint: None,
                advisory: Some(false),
                row_seqs: vec![i as u64],
                rule_sourced: false,
            })
        })
        .collect();
    SeqDoc {
        title: l.scenario.clone(),
        description: None,
        passed: l.arrows.iter().all(|a| a.errors.is_empty()),
        lanes,
        rows,
        anomalies,
        views: Vec::new(),
        epoch_base_ms: None,
    }
}

/// The arrows as a table, the "other" row, and the failures with no arrow.
fn table(out: &mut String, l: &Ladder) {
    out.push_str(
        "<table><tr><th>from → to</th><th>message</th><th>calls</th><th>mean T+</th>\
         <th>retr</th><th>resent</th><th>errors</th></tr>",
    );
    let row = |out: &mut String, route: String, a: &LadderArrow| {
        let errors = classes(&a.errors);
        let cls = if errors.is_empty() { "" } else { " class=\"nok\"" };
        let _ = write!(
            out,
            "<tr><td>{}</td><td>{}</td><td>{}</td><td>{:.0} ms</td><td>{}</td><td>{}</td>\
             <td{cls}>{}</td></tr>",
            esc(&route),
            esc(&name(a)),
            a.count,
            a.mean_offset_ms,
            a.retransmissions,
            a.resent,
            esc(&errors)
        );
    };
    for a in &l.arrows {
        row(out, format!("{} → {}", a.from, a.to), a);
    }
    if let Some(other) = &l.other {
        row(out, "other arrows".to_string(), other);
    }
    out.push_str("</table>");
    let mut notes = Vec::new();
    if !l.before_reply.is_empty() {
        notes.push(format!("before any message in: {}", classes(&l.before_reply)));
    }
    if !l.after_call.is_empty() {
        notes.push(format!("after the call: {}", classes(&l.after_call)));
    }
    if !notes.is_empty() {
        let _ = write!(out, "<p class=\"nok\">{}</p>", esc(&notes.join("; ")));
    }
}
