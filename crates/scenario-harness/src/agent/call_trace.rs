//! The per-call trace a SUT recorded on the trace plane (ADR-0026) — every
//! `sip.in`, `rule.fired`, `rule.transition`, `call.transition`, timer and
//! limiter fact of each sampled call — rendered for a failure dump: grouped
//! per call, in order, one compact line per fact. Any trace kind a SUT adds
//! (its timer arm/cancel/fire facts included) appears here with no change.

use observe::CapturedEvent;

use crate::report::wire::format_relative;

/// How many calls a dump shows: the most recent ones.
const MAX_CALLS: usize = 8;
/// How many facts a dump shows per call: the tail, nearest the failure.
const MAX_FACTS_PER_CALL: usize = 80;

/// The value of the captured field `name`.
fn field<'e>(e: &'e CapturedEvent, name: &str) -> Option<&'e str> {
    e.fields.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
}

/// One fact as a line: when, what kind, its detail, and for a SIP message the
/// first line of its bytes.
fn fact_line(e: &CapturedEvent, kind: &str) -> String {
    let at = field(e, "at_ms").and_then(|v| v.parse::<i64>().ok()).map(format_relative);
    let detail = field(e, "detail").unwrap_or_default();
    let first_line = match kind {
        "sip.in" | "sip.out" => field(e, "body")
            .and_then(|b| b.lines().next())
            .filter(|l| !l.is_empty())
            .map(|l| format!("  {l}"))
            .unwrap_or_default(),
        _ => String::new(),
    };
    format!("[{}] {kind:<16} {detail}{first_line}", at.unwrap_or_else(|| "?".to_string()))
}

/// The trace-plane facts among `events`, one block per call in the order the
/// calls first appear, each block its facts in emission order.
pub(super) fn render_call_traces(events: &[CapturedEvent]) -> Vec<String> {
    let mut calls: Vec<(String, Vec<String>)> = Vec::new();
    for e in events.iter().filter(|e| observe::is_trace_plane(&e.target)) {
        let Some(kind) = field(e, "kind") else { continue };
        let call = e.call.clone().unwrap_or_else(|| "(no call)".to_string());
        let line = fact_line(e, kind);
        match calls.iter_mut().find(|(c, _)| *c == call) {
            Some((_, lines)) => lines.push(line),
            None => calls.push((call, vec![line])),
        }
    }
    if calls.is_empty() {
        return vec!["(no call was traced)".to_string()];
    }
    let mut out = Vec::new();
    let elided_calls = calls.len().saturating_sub(MAX_CALLS);
    if elided_calls > 0 {
        out.push(format!("… {elided_calls} earlier call(s) elided"));
    }
    for (call, lines) in calls.iter().skip(elided_calls) {
        out.push(format!("call Call-ID {call} ({} fact(s))", lines.len()));
        let elided = lines.len().saturating_sub(MAX_FACTS_PER_CALL);
        if elided > 0 {
            out.push(format!("  … {elided} earlier fact(s) elided"));
        }
        out.extend(lines.iter().skip(elided).map(|l| format!("  {l}")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(call: &str, kind: &str, at_ms: i64, detail: &str, body: &str) -> CapturedEvent {
        CapturedEvent {
            level: "INFO".to_string(),
            target: observe::TRACE_TARGET.to_string(),
            message: "trace".to_string(),
            fields: vec![
                ("kind".to_string(), kind.to_string()),
                ("at_ms".to_string(), at_ms.to_string()),
                ("detail".to_string(), detail.to_string()),
                ("body".to_string(), body.to_string()),
            ],
            call: Some(call.to_string()),
        }
    }

    #[test]
    fn facts_are_grouped_per_call_in_order_one_line_each() {
        let events = vec![
            fact("c1", "sip.in", 0, "from 127.0.0.1:5060", "INVITE sip:bob@h SIP/2.0\r\nVia: x"),
            fact("c2", "sip.in", 5, "from 127.0.0.1:5061", "OPTIONS sip:h SIP/2.0\r\n"),
            fact("c1", "rule.fired", 1, "route-initial-invite", ""),
        ];
        let lines = render_call_traces(&events);
        assert_eq!(lines[0], "call Call-ID c1 (2 fact(s))");
        assert!(lines[1].contains("sip.in") && lines[1].ends_with("INVITE sip:bob@h SIP/2.0"));
        assert!(lines[2].contains("rule.fired") && lines[2].contains("route-initial-invite"));
        assert_eq!(lines[3], "call Call-ID c2 (1 fact(s))");
    }

    #[test]
    fn a_long_call_shows_its_tail() {
        let events: Vec<_> = (0..(MAX_FACTS_PER_CALL + 3))
            .map(|i| fact("c", "rule.fired", i as i64, "r", ""))
            .collect();
        let lines = render_call_traces(&events);
        assert_eq!(lines[1], "  … 3 earlier fact(s) elided");
        assert_eq!(lines.len(), MAX_FACTS_PER_CALL + 2);
    }
}
