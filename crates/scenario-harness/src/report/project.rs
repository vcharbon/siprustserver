//! Projector: SIP recording → neutral [`seq_report::SeqDoc`].
//!
//! This is the scenario-harness half of the shared-renderer contract (the
//! unification described in `seq-report`'s crate docs). The renderer knows
//! nothing about SIP; this module owns the SIP→neutral translation so the SIP
//! HTML/global.txt artifacts are now produced by the SHARED renderer over a
//! `SeqDoc` — identical machinery to the failover harness's three-plane view,
//! just with a single (SIP) plane.
//!
//! Lane identity is `(ip,port)` (the recorder's `Lane::addr`); a lane's id is
//! its address string so `RecordedSipEntry::{from,to}` map straight onto lane
//! ids. Wire text is carried as each row's expandable `detail`, so the historic
//! per-message wire dump survives verbatim in both artifacts.

use layer_harness::{Lane as RecLane, NetworkTag, RecordedScenario};
use seq_report::{Anomaly, Lane, LaneKind, RowKind, SeqDoc, SeqRow};
use sip_net::RecordedSipEntry;

use super::wire::{facets, wire_text};

/// Build a single-plane (SIP) [`SeqDoc`] from a run's recording.
///
/// `extra_anomalies` are folded into the doc on top of the recorder's structural
/// anomalies — the RFC 3261 CSeq hard-gate findings are passed here so the report
/// shows FAIL and lists the violation whenever the trace breaks the rule. If
/// `extra_anomalies` is non-empty the doc is forced `passed = false`: a trace
/// that violates the RFC can NEVER render PASS.
pub fn sip_doc(
    scenario_name: &str,
    description: Option<&str>,
    entries: &[RecordedSipEntry],
    scenario: &RecordedScenario,
    passed: bool,
    extra_anomalies: &[Anomaly],
) -> SeqDoc {
    let lanes = project_lanes(&scenario.lanes, entries);
    let base = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);
    let rows = entries.iter().map(|e| project_entry(e, base)).collect();
    let anomalies: Vec<Anomaly> = scenario
        .anomalies
        .iter()
        .map(|a| Anomaly {
            check: a.check.clone(),
            detail: a.detail.clone(),
            lane: a.bind_key.clone(),
            endpoint: None,
            // The structural layer-close kinds (queueLeak, inFlightImbalance,
            // undeliverable) are deliberately never gated by the harness even
            // when their recorded severity is deferred-fail (timeout / reap
            // fixtures legitimately produce them) — surface them as advisory.
            // Only a signalingAudit (RFC-rule) entry carries a gating severity.
            advisory: Some(a.kind != "signalingAudit" || !a.severity.fails()),
        })
        .collect();
    // The recorder's native findings and the re-folded `extra_anomalies` (same
    // rule set, run again for the `run.rs` path that wires none into the
    // recorder) overlap on the `agent.rs` path — same rule name, detail, and
    // lane. Collapse exact duplicates so a finding is listed once, walking the
    // EXTRAS first: the evaluator's advisory tag is authoritative when both
    // carry the same finding.
    let mut deduped: Vec<Anomaly> = Vec::with_capacity(anomalies.len() + extra_anomalies.len());
    let mut seen = std::collections::HashSet::new();
    for a in extra_anomalies.iter().cloned().chain(anomalies.into_iter()) {
        if seen.insert((a.check.clone(), a.detail.clone(), a.lane.clone())) {
            deduped.push(a);
        }
    }
    let mut anomalies = deduped;

    // Resolve each finding's lane to its registered display name (`lb`,
    // `bob1`, …) so report tables can tag rows with the endpoint, not just an
    // ip:port the reader has to cross-reference.
    let name_of: std::collections::HashMap<&str, &str> = lanes
        .iter()
        .map(|l| (l.id.as_str(), l.label.as_str()))
        .collect();
    for a in &mut anomalies {
        if a.endpoint.is_none() {
            if let Some(lane) = &a.lane {
                a.endpoint = name_of.get(lane.as_str()).map(|n| {
                    // `label` is `name (ip:port)` — keep the bare name half.
                    n.split(" (").next().unwrap_or(n).to_string()
                });
            }
        }
    }

    // A trace with GATING RFC violations can never render PASS, regardless of
    // the caller's `passed` (the fluent harness reports `passed = true` by
    // construction). Advisory findings are informational and do not flip it.
    let gating = anomalies.iter().any(|a| a.is_gating());

    SeqDoc {
        title: scenario_name.to_string(),
        description: description.map(str::to_string),
        passed: passed && !gating,
        lanes,
        rows,
        anomalies,
    }
}

/// Project the recorder lanes into seq-report columns. A `proxy`/`core` lane is
/// an SUT; everything else is a UA. Any address that appears in the trace but
/// was never registered as a lane (rare) is appended so its rows resolve.
fn project_lanes(rec_lanes: &[RecLane], entries: &[RecordedSipEntry]) -> Vec<Lane> {
    let mut lanes: Vec<Lane> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for l in rec_lanes {
        let id = l.addr.to_string();
        let name = l.names.first().cloned().unwrap_or_default();
        let label = if name.is_empty() {
            id.clone()
        } else {
            format!("{name} ({id})")
        };
        // A core-fabric endpoint (the proxy) is an SUT; an ext UA is a Ua.
        let kind = match l.network {
            NetworkTag::Core => LaneKind::Sut,
            NetworkTag::Ext if name == "proxy" => LaneKind::Sut,
            NetworkTag::Ext => LaneKind::Ua,
        };
        if seen.insert(id.clone()) {
            lanes.push(Lane::new(id, label, kind));
        }
    }

    // Backstop: any addr referenced by a row but missing a lane.
    for e in entries {
        for addr in [e.from, e.to] {
            let id = addr.to_string();
            if seen.insert(id.clone()) {
                lanes.push(Lane::new(id.clone(), id, LaneKind::Ua));
            }
        }
    }

    lanes
}

fn project_entry(e: &RecordedSipEntry, base: i64) -> SeqRow {
    // Carry the transit (sent → received) as the first detail line whenever the
    // message actually crossed with a delay; a zero-transit / undelivered entry
    // shows only the single stamp. This preserves the historic text report's
    // two-timestamp transit display now that the global view is rendered by the
    // shared (plane-neutral) renderer, which keys off a single `at_ms`.
    let mut detail = String::new();
    if let Some(rcvd) = e.received_ms.filter(|r| *r != e.sent_ms) {
        detail.push_str(&format!(
            "┄ sent {} → rcvd {}\n",
            seq_report::format_relative(e.sent_ms as i64 - base),
            seq_report::format_relative(rcvd as i64 - base),
        ));
    }
    detail.push_str(&wire_text(&e.raw));

    SeqRow {
        at_ms: e.sent_ms as i64,
        seq: e.seq,
        from: e.from.to_string(),
        to: Some(e.to.to_string()),
        label: facets(&e.raw).label,
        detail: Some(detail),
        conn: None,
        kind: RowKind::Sip {
            delivered: e.delivered,
        },
    }
}
