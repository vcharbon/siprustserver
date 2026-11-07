//! The text layout of call groups: one summary line per group, or its ladder,
//! one line per message in capture order, the raw bytes under it on demand.

use std::borrow::Cow;

use sip_message::SipMessage;
use sip_pcap::flow::{CallGroup, FlowLeg};

pub(crate) fn fmt_ts(ts_us: u64) -> String {
    let secs = ts_us / 1_000_000;
    let ms = (ts_us % 1_000_000) / 1_000;
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

fn leg_label(pos: usize) -> String {
    match pos {
        0 => "A".to_string(),
        n => format!("B{n}"),
    }
}

fn summary_line(msg: &SipMessage) -> String {
    match msg {
        SipMessage::Request(r) => format!(
            "{} {} (CSeq {} {})",
            r.method(),
            r.request_uri().text(),
            r.cseq().seq(),
            r.cseq().method()
        ),
        SipMessage::Response(r) => {
            format!("{} {} (CSeq {} {})", r.status(), r.reason(), r.cseq().seq(), r.cseq().method())
        }
    }
}

fn final_str(leg: &FlowLeg) -> String {
    match (leg.invite.is_some(), leg.final_status) {
        (false, _) => "-".to_string(),
        (true, Some(c)) => c.to_string(),
        (true, None) => "NONE".to_string(),
    }
}

fn invite_strs(leg: &FlowLeg) -> (Cow<'_, str>, Cow<'_, str>, Cow<'_, str>) {
    match &leg.invite {
        Some(inv) => (inv.ruri.text(), inv.from_uri.text(), inv.to_uri.text()),
        None => (Cow::Borrowed("-"), Cow::Borrowed("-"), Cow::Borrowed("-")),
    }
}

pub(crate) fn print_list_line(n: usize, group: &CallGroup, legs: &[FlowLeg]) {
    let a = &legs[group.legs[0]];
    let a_tokens = a.tokens();
    let token = a_tokens.iter().next().copied().unwrap_or("-");
    let (ruri, from, to) = invite_strs(a);
    let dur_ms = (legs[group.legs.last().copied().unwrap_or(group.legs[0])].t_last())
        .saturating_sub(a.t_first())
        / 1_000;
    println!(
        "#{n:<4} {} legs={} token={token} ruri={ruri} from={from} to={to} finals=[{}] ring={} term={} dur={dur_ms}ms msgs={}",
        fmt_ts(a.t_first()),
        group.legs.len(),
        group.legs.iter().map(|&l| final_str(&legs[l])).collect::<Vec<_>>().join(","),
        group.legs.iter().any(|&l| legs[l].saw_180),
        group
            .legs
            .iter()
            .filter_map(|&l| legs[l].terminated_by)
            .next()
            .map(|t| t.as_str())
            .unwrap_or("-"),
        group.legs.iter().map(|&l| legs[l].msgs.len()).sum::<usize>(),
    );
}

pub(crate) fn print_ladder(n: usize, group: &CallGroup, legs: &[FlowLeg], full: bool) {
    let t0 = legs[group.legs[0]].t_first();
    println!("\n━━━ call group #{n} ─ {} ─ legs={} ━━━", fmt_ts(t0), group.legs.len());
    for (pos, &l) in group.legs.iter().enumerate() {
        let leg = &legs[l];
        let tokens: Vec<&str> = leg.tokens().into_iter().collect();
        let (ruri, from, to) = invite_strs(leg);
        println!(
            "  leg {}: Call-ID {}\n         INVITE {}  {} -> {}\n         final: {}  term: {}  msgs: {}{}",
            leg_label(pos),
            leg.call_id,
            ruri,
            from,
            to,
            final_str(leg),
            leg.terminated_by.map(|t| t.as_str()).unwrap_or("-"),
            leg.msgs.len(),
            if tokens.is_empty() { String::new() } else { format!("  token: {}", tokens.join(",")) },
        );
    }
    // Merged chronological ladder across all legs of the group.
    let mut lines: Vec<(u64, String, Option<String>)> = Vec::new();
    for (pos, &l) in group.legs.iter().enumerate() {
        let leg = &legs[l];
        for m in &leg.msgs {
            let line = format!(
                "  {:>9.3}s {} {:>21} → {:<21} [{}] {}{}",
                (m.ts_us.saturating_sub(t0)) as f64 / 1e6,
                fmt_ts(m.ts_us),
                m.src.to_string(),
                m.dst.to_string(),
                leg_label(pos),
                summary_line(&m.parsed),
                if m.retx { "  (retx)" } else { "" },
            );
            let body = full.then(|| {
                String::from_utf8_lossy(m.raw())
                    .lines()
                    .map(|l| format!("      | {l}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            });
            lines.push((m.ts_us, line, body));
        }
    }
    lines.sort_by_key(|line| line.0);
    for (_, line, body) in lines {
        println!("{line}");
        if let Some(b) = body {
            println!("{b}");
        }
    }
}
