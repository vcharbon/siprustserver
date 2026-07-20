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

/// Wall-clock + chaos-marker overlay for a rendered callflow.
///
/// Empty ([`TimelineOverlay::default`]) on the paused-clock harness path (the
/// recording's `at_ms` is virtual time there, and there are no externally-flagged
/// faults). The load driver supplies a populated overlay: its recording rides
/// `Clock::system()` so `at_ms` is real wall-clock epoch ms (`wall_clock = true`
/// → the doc renders absolute UTC), and `markers` are the injected-fault instants
/// flagged via `POST /chaos` (rendered as `Lifecycle` bands so a NOK flow shows
/// exactly when the kill landed relative to its frames).
#[derive(Default, Clone)]
pub struct TimelineOverlay {
    /// `true` when `RecordedSipEntry::sent_ms` is real wall-clock epoch ms, so
    /// the rendered doc carries an absolute-UTC reference next to each `T+…`.
    pub wall_clock: bool,
    /// Injected-fault markers as `(wall_clock_epoch_ms, label)`. Filtered to the
    /// call's timeline window and emitted as `RowKind::Lifecycle` bands.
    pub markers: Vec<(i64, String)>,
}

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
    sip_doc_with_overlay(
        scenario_name,
        description,
        entries,
        scenario,
        passed,
        extra_anomalies,
        &TimelineOverlay::default(),
    )
}

/// [`sip_doc`] plus a wall-clock/chaos [`TimelineOverlay`] — the load-driver entry
/// point. Adds an absolute-UTC reference (when `overlay.wall_clock`) and renders
/// each in-window chaos marker as a `Lifecycle` band positioned at the kill
/// instant on the call's own timeline.
pub fn sip_doc_with_overlay(
    scenario_name: &str,
    description: Option<&str>,
    entries: &[RecordedSipEntry],
    scenario: &RecordedScenario,
    passed: bool,
    extra_anomalies: &[Anomaly],
    overlay: &TimelineOverlay,
) -> SeqDoc {
    let lanes = project_lanes(&scenario.lanes, entries);
    let base = entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);
    let mut rows: Vec<SeqRow> = entries.iter().map(|e| project_entry(e, base)).collect();

    // Overlay the injected-fault markers as Lifecycle bands. Only those within
    // the call's timeline (± a small margin so a kill just before setup or just
    // after the last frame still shows) are rendered, so an unrelated kill from
    // another call's window doesn't clutter this flow. A marker is positioned by
    // borrowing the `seq` of the last frame at/▸before it (so the shared
    // `(seq, at_ms)` sort drops the band exactly between the surrounding frames).
    if !overlay.markers.is_empty() && !entries.is_empty() {
        const MARGIN_MS: i64 = 2_000;
        let last = entries
            .iter()
            .map(|e| e.received_ms.unwrap_or(e.sent_ms) as i64)
            .max()
            .unwrap_or(base);
        let (lo, hi) = (base - MARGIN_MS, last + MARGIN_MS);
        for (wall_ms, label) in &overlay.markers {
            let at = *wall_ms;
            if at < lo || at > hi {
                continue;
            }
            // seq of the last frame at/before the marker (0 if it precedes all).
            let seq = entries
                .iter()
                .filter(|e| (e.sent_ms as i64) <= at)
                .map(|e| e.seq)
                .max()
                .unwrap_or(0);
            rows.push(SeqRow {
                at_ms: at,
                seq,
                from: String::new(),
                to: None,
                label: label.clone(),
                detail: None,
                conn: None,
                kind: RowKind::Lifecycle,
            });
        }
    }
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
    for a in extra_anomalies.iter().cloned().chain(anomalies) {
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

    // When the recording rides the system wall clock (`at_ms` is real epoch ms),
    // anchor the doc to it so the renderers show absolute UTC. `at_ms == base`
    // already IS the epoch, so the anchor is simply `base`.
    let epoch_base_ms = overlay.wall_clock.then_some(base);

    SeqDoc {
        title: scenario_name.to_string(),
        description: description.map(str::to_string),
        passed: passed && !gating,
        lanes,
        rows,
        anomalies,
        epoch_base_ms,
    }
}

/// The NORMALIZED projection of a run's report doc (ADR-0024 §7) — the
/// functional-lane entry point into the ONE shared [`seq_report::normalize`], so
/// a plan's report is comparable byte-for-byte against the load lane's
/// ([`crate::AgentBinder::normalized_seq_doc`]). Both lanes derive the lane-id →
/// role map the same way (from the doc's own lanes).
pub fn normalized_sip_doc(
    scenario_name: &str,
    description: Option<&str>,
    entries: &[RecordedSipEntry],
    scenario: &RecordedScenario,
    passed: bool,
    extra_anomalies: &[Anomaly],
) -> SeqDoc {
    let doc = sip_doc(scenario_name, description, entries, scenario, passed, extra_anomalies);
    normalize_doc(&doc)
}

/// Normalize an already-built [`SeqDoc`] through the shared projection.
pub fn normalize_doc(doc: &SeqDoc) -> SeqDoc {
    seq_report::normalize(doc, &seq_report::role_map_from_lanes(&doc.lanes))
}

/// Project the recorder lanes into seq-report columns. A `proxy`/`core` lane is
/// an SUT; everything else is a UA. Any address that appears in the trace but
/// was never registered as a lane (rare) is appended so its rows resolve.
///
/// Lane identity is the recorder's lane KEY — the bare `ip:port`, or the
/// logical-sub-lane `ip:port#<endpoint>` form (036 ask C) when several logical
/// endpoints share one socket. Sub-lanes carry a `group` (the shared `ip:port`)
/// so the renderer brackets them under one socket header.
fn project_lanes(rec_lanes: &[RecLane], entries: &[RecordedSipEntry]) -> Vec<Lane> {
    let mut lanes: Vec<Lane> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for l in rec_lanes {
        let id = l.key.clone();
        let addr = l.addr.to_string();
        let sublane = id.contains('#');
        let name = l.names.first().cloned().unwrap_or_default();
        // A sub-lane sits under its socket's group header (which shows the
        // ip:port), so its own caption is just the endpoint name.
        let label = match (sublane, name.is_empty()) {
            (true, false) => name.clone(),
            (true, true) => id.split('#').nth(1).unwrap_or(&id).to_string(),
            (false, false) => format!("{name} ({addr})"),
            (false, true) => addr.clone(),
        };
        // A core-fabric endpoint (the proxy) is an SUT; an ext UA is a Ua.
        let kind = match l.network {
            NetworkTag::Core => LaneKind::Sut,
            NetworkTag::Ext if name == "proxy" => LaneKind::Sut,
            NetworkTag::Ext => LaneKind::Ua,
        };
        if seen.insert(id.clone()) {
            let mut lane = Lane::new(id, label, kind);
            if sublane {
                lane = lane.with_group(addr);
            }
            lanes.push(lane);
        }
    }

    // Backstop: any lane id referenced by a row but never registered — an
    // unregistered address, or the synthesized `ip:port#noendpoint` sub-lane
    // an unrouted-but-call-correlated datagram renders on (036 ask C).
    for e in entries {
        let (from_id, to_id) = entry_lane_ids(e);
        for id in [from_id, to_id] {
            if seen.insert(id.clone()) {
                let lane = match id.split_once('#') {
                    Some((addr, "noendpoint")) => {
                        Lane::new(id.clone(), "(no endpoint)", LaneKind::Ua).with_group(addr)
                    }
                    Some((addr, label)) => {
                        Lane::new(id.clone(), label, LaneKind::Ua).with_group(addr)
                    }
                    None => Lane::new(id.clone(), id.clone(), LaneKind::Ua),
                };
                // Keep sub-lanes of one socket adjacent so the group brackets.
                let insert_at = lane
                    .group
                    .as_deref()
                    .and_then(|g| {
                        lanes
                            .iter()
                            .rposition(|l| l.group.as_deref() == Some(g))
                            .map(|i| i + 1)
                    })
                    .unwrap_or(lanes.len());
                lanes.insert(insert_at, lane);
            }
        }
    }

    lanes
}

/// The seq-report lane ids a recorded entry's row endpoints resolve to: the
/// full lane key when the recording knows it, the bare address otherwise, and
/// the synthesized `ip:port#noendpoint` sub-lane for a datagram that
/// correlated to the call but had no live logical endpoint to land on.
fn entry_lane_ids(e: &RecordedSipEntry) -> (String, String) {
    let from = e.from_lane.clone().unwrap_or_else(|| e.from.to_string());
    let to = if e.recv_note == Some(sip_net::RecvNote::Unrouted) {
        format!("{}#noendpoint", e.to)
    } else {
        e.to_lane.clone().unwrap_or_else(|| e.to.to_string())
    };
    (from, to)
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
    // True-wire receive note (newkahneed-036 ask A): a message that arrived but
    // was never consumed by the scenario body (or was refused by the inbox /
    // discarded by the loadgen loss model) is visibly distinguishable from one
    // the body expected and matched — on the arrow label AND in the detail.
    let f = facets(&e.raw);
    let mut label = f.label;
    if let Some(note) = e.recv_note {
        label.push_str(&format!(" ⚠ [{}]", note.tag()));
        detail.push_str(&format!("┄ receive note: {}\n", note.tag()));
    }
    // Outbound re-emission (loadgen retransmit engine): tag it so recovery
    // traffic reads distinctly from a first transmission (always-display
    // re-emission).
    if let Some(kind) = e.reemit {
        label.push_str(&format!(" ⟳ [{}]", kind.tag()));
        detail.push_str(&format!("┄ re-emit: {}\n", kind.tag()));
    }
    detail.push_str(&wire_text(&e.raw));

    let (from, to) = entry_lane_ids(e);
    SeqRow {
        at_ms: e.sent_ms as i64,
        seq: e.seq,
        from,
        to: Some(to),
        label,
        detail: Some(detail),
        // Colour-band per dialog (036 ask C): rows sharing a Call-ID share a
        // hue, so a b2bua's a-leg vs b-leg — or a reroute's primary vs alt
        // dialog — read as distinct flows at a glance.
        conn: (!f.call_id.is_empty()).then(|| f.call_id.clone()),
        kind: RowKind::Sip {
            delivered: e.delivered,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use layer_harness::TransportKind;

    fn entry(from: &str, to: &str, raw: &str, sent_ms: u64, seq: u64) -> RecordedSipEntry {
        RecordedSipEntry {
            from: from.parse().unwrap(),
            to: to.parse().unwrap(),
            raw: raw.as_bytes().to_vec(),
            sent_ms,
            received_ms: Some(sent_ms + 1),
            delivered: true,
            recv_note: None,
            reemit: None,
            from_lane: None,
            to_lane: None,
            seq,
        }
    }

    fn scenario() -> RecordedScenario {
        RecordedScenario {
            transport_kind: TransportKind::Live,
            lanes: vec![],
            anomalies: vec![],
        }
    }

    /// 036 ask C: registered sub-lanes keep their composite key as the lane id
    /// and group under the shared socket; an `Unrouted` entry lands on a
    /// synthesized `ip:port#noendpoint` sub-lane adjacent to its siblings; and
    /// rows colour-band by Call-ID via `conn`.
    #[test]
    fn sub_lanes_noendpoint_and_call_id_bands() {
        let sock: std::net::SocketAddr = "10.0.0.2:5070".parse().unwrap();
        let mut scenario = scenario();
        scenario.lanes = vec![
            layer_harness::Lane {
                key: "10.0.0.1:5060".into(),
                addr: "10.0.0.1:5060".parse().unwrap(),
                names: vec!["alice".into()],
                network: NetworkTag::Ext,
                killed_at: vec![],
            },
            layer_harness::Lane {
                key: "10.0.0.2:5070#callee".into(),
                addr: sock,
                names: vec!["callee".into()],
                network: NetworkTag::Ext,
                killed_at: vec![],
            },
        ];

        let raw = "INVITE sip:x@h SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bKt1\r\n\
             Call-ID: cid-1@h\r\n\
             From: <sip:a@h>;tag=1\r\nTo: <sip:x@h>\r\nCSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n";
        let mut routed = entry("10.0.0.1:5060", "10.0.0.2:5070", raw, 10, 1);
        routed.from_lane = Some("10.0.0.1:5060".into());
        routed.to_lane = Some("10.0.0.2:5070#callee".into());
        let mut unrouted = entry("10.0.0.1:5060", "10.0.0.2:5070", raw, 20, 2);
        unrouted.recv_note = Some(sip_net::RecvNote::Unrouted);

        let doc = sip_doc("lanes", None, &[routed, unrouted], &scenario, true, &[]);

        let callee = doc.lanes.iter().find(|l| l.id == "10.0.0.2:5070#callee").unwrap();
        assert_eq!(callee.label, "callee");
        assert_eq!(callee.group.as_deref(), Some("10.0.0.2:5070"));

        let noep = doc.lanes.iter().find(|l| l.id == "10.0.0.2:5070#noendpoint").unwrap();
        assert_eq!(noep.label, "(no endpoint)");
        assert_eq!(noep.group.as_deref(), Some("10.0.0.2:5070"));
        // Adjacent to its sibling sub-lane so the group header brackets both.
        let idx_of = |id: &str| doc.lanes.iter().position(|l| l.id == id).unwrap();
        assert_eq!(
            idx_of("10.0.0.2:5070#noendpoint"),
            idx_of("10.0.0.2:5070#callee") + 1
        );

        // Rows resolve to the sub-lane ids, and colour-band by Call-ID.
        assert_eq!(doc.rows[0].to.as_deref(), Some("10.0.0.2:5070#callee"));
        assert_eq!(doc.rows[1].to.as_deref(), Some("10.0.0.2:5070#noendpoint"));
        assert!(doc.rows[1].label.contains("no endpoint to route to"));
        assert_eq!(doc.rows[0].conn.as_deref(), Some("cid-1@h"));
    }

    #[test]
    fn overlay_renders_in_window_marker_as_lifecycle_band_with_wall_epoch() {
        // Two frames at wall-clock ms t0 and t0+10s.
        let t0 = 1_782_802_100_000i64;
        let entries = vec![
            entry("10.0.0.1:5060", "10.0.0.2:5060", "INVITE sip:bob@x SIP/2.0\r\n", t0 as u64, 1),
            entry("10.0.0.1:5060", "10.0.0.2:5060", "BYE sip:bob@x SIP/2.0\r\n", (t0 + 10_000) as u64, 9),
        ];
        // One kill inside the call window (t0 + 4s), one far outside (t0 + 60s).
        let overlay = TimelineOverlay {
            wall_clock: true,
            markers: vec![
                (t0 + 4_000, "chaos kill_worker(b2bua-worker-0)".to_string()),
                (t0 + 60_000, "chaos kill_worker(other-window)".to_string()),
            ],
        };
        let doc = sip_doc_with_overlay("reinvite", None, &entries, &scenario(), false, &[], &overlay);

        // Wall-clock anchor set → renders absolute UTC.
        assert_eq!(doc.epoch_base_ms, Some(t0));
        // Exactly one Lifecycle band — the in-window kill; the far one is dropped.
        let bands: Vec<&SeqRow> =
            doc.rows.iter().filter(|r| r.kind == RowKind::Lifecycle).collect();
        assert_eq!(bands.len(), 1, "only the in-window marker becomes a band");
        assert_eq!(bands[0].at_ms, t0 + 4_000);
        assert!(bands[0].label.contains("kill_worker(b2bua-worker-0)"));
        // Positioned after the INVITE (seq 1) and before the BYE (seq 9) so the
        // shared (seq, at_ms) sort drops it between them.
        assert_eq!(bands[0].seq, 1, "borrows the preceding frame's seq");

        // The rendered HTML carries both the absolute anchor and the band label.
        let html = seq_report::render_html(&doc);
        assert!(html.contains("Timeline t0"));
        assert!(html.contains("kill_worker(b2bua-worker-0)"));
    }

    #[test]
    fn no_overlay_is_relative_only() {
        let entries = vec![entry(
            "10.0.0.1:5060",
            "10.0.0.2:5060",
            "INVITE sip:bob@x SIP/2.0\r\n",
            1_782_802_100_000,
            1,
        )];
        let doc = sip_doc("basic_call", None, &entries, &scenario(), true, &[]);
        assert_eq!(doc.epoch_base_ms, None, "default path stays virtual/relative");
        assert!(!doc.rows.iter().any(|r| r.kind == RowKind::Lifecycle));
    }
}
