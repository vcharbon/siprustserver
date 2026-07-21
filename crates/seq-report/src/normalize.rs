//! The normalized report projection (ADR-0024 §7): one shared function that
//! reduces a [`SeqDoc`] to a lane-, timing- and wire-independent canonical form,
//! so "the same plan produces the same report on every lane" is a byte
//! comparison. Every lane's projector calls [`normalize`] — it is NOT
//! per-lane-duplicated.
//!
//! What normalization removes (the axes that legitimately differ across lanes):
//! - lane ids/labels → ROLE names (real lanes bind ephemeral ports; the fake
//!   lane does not) — no addresses, no `ip:port#name` sub-lane keys, no ports;
//! - row labels → method/status only; `detail` (wire text) and `conn` (socket
//!   ids) dropped;
//! - retransmission rows collapse onto their first occurrence (real transport
//!   retransmits; the fake lane does not) and per-attempt `delivered` is forced
//!   true (delivery is a transport fact, not a plan fact);
//! - `at_ms` (wall/virtual timing) → the causal ordinal; `seq` is re-based onto
//!   the same ordinal so two lanes' differing absolute sequence numbers agree;
//! - `Lifecycle` bands (chaos markers — load-lane only, wall-clock placed) are
//!   dropped;
//! - only normalization-stable anomalies survive: RFC-audit findings (keyed by
//!   rule id) are kept with their lane mapped to a role and their timing-bearing
//!   `detail` cleared; every other anomaly (the structural layer-close kinds —
//!   queueLeak / inFlightImbalance / undeliverable — and the call-result /
//!   check-verdict notes, all of which carry timing or transport specifics) is
//!   dropped.

use std::collections::HashMap;

use crate::{Anomaly, Lane, RowKind, SeqDoc, SeqRow};

/// Label substrings the SIP projector stamps to mark a retransmission — an
/// outbound timer re-emit or an absorbed inbound duplicate. A row carrying one
/// collapses onto the first (un-marked) occurrence.
const RETRANSMIT_MARKERS: [&str; 2] = ["\u{21bb}", "absorbed retransmit"];

/// Derive a lane-id → role-name map from a doc's lanes, the way BOTH lane
/// projectors do: a lane's role is the name half of its label (`name (ip:port)`
/// → `name`; a sub-lane's label IS the bare name), with an address-shaped label
/// falling back to the lane kind so no `ip:port` ever survives as a role.
pub fn role_map_from_lanes(lanes: &[Lane]) -> HashMap<String, String> {
    lanes.iter().map(|l| (l.id.clone(), role_of_lane(l))).collect()
}

fn role_of_lane(lane: &Lane) -> String {
    // `label` is `name (ip:port)` (registered UA), a bare `name` (sub-lane), or
    // the raw `ip:port` (an unnamed backstop lane). Keep the name half.
    let name = lane.label.split(" (").next().unwrap_or(&lane.label).trim();
    if name.is_empty() || looks_like_addr(name) {
        // An unnamed / address-only lane: fall back to its kind so a role never
        // carries an address.
        return match lane.kind {
            crate::LaneKind::Sut => "sut".to_string(),
            crate::LaneKind::Node => "node".to_string(),
            crate::LaneKind::Ua => "ua".to_string(),
        };
    }
    name.to_string()
}

/// Whether a string is host:port-shaped (a trailing `:<digits>`), so an
/// address never leaks into a normalized role.
fn looks_like_addr(s: &str) -> bool {
    match s.rsplit_once(':') {
        Some((host, port)) => !host.is_empty() && !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        None => false,
    }
}

/// Normalize a [`SeqDoc`] to its lane-, timing- and wire-independent canonical
/// form (see the module docs). `role_map` maps each lane id to a role name (see
/// [`role_map_from_lanes`]); an id absent from the map falls back to the lane's
/// derived role. Two lanes AGREE on a plan when their normalized docs serialize
/// identically. Deterministic and idempotent.
pub fn normalize(doc: &SeqDoc, role_map: &HashMap<String, String>) -> SeqDoc {
    let resolve = |id: &str| -> String {
        role_map
            .get(id)
            .cloned()
            .unwrap_or_else(|| role_of_lane_id(id))
    };

    // Lanes → roles, order-preserving, deduplicated on the resolved role.
    let mut lanes: Vec<Lane> = Vec::new();
    let mut seen_lane = std::collections::HashSet::new();
    for l in &doc.lanes {
        let role = resolve(&l.id);
        if seen_lane.insert(role.clone()) {
            lanes.push(Lane::new(role.clone(), role, l.kind));
        }
    }

    // Rows: drop lifecycle bands and retransmissions; canonicalize the rest.
    let mut kept: Vec<&SeqRow> = doc
        .rows
        .iter()
        .filter(|r| !matches!(r.kind, RowKind::Lifecycle))
        .filter(|r| !is_retransmit(&r.label))
        .collect();
    // Causal order: sort by the recording sequence (the cross-plane truth), then
    // re-base at_ms AND seq onto a dense ordinal so absolute numbers never differ.
    kept.sort_by(|a, b| a.seq.cmp(&b.seq).then(a.at_ms.cmp(&b.at_ms)));
    let rows: Vec<SeqRow> = kept
        .into_iter()
        .enumerate()
        .map(|(i, r)| SeqRow {
            at_ms: i as i64,
            seq: i as u64,
            from: resolve(&r.from),
            to: r.to.as_deref().map(&resolve),
            label: normalize_label(&r.label),
            detail: None,
            conn: None,
            kind: force_delivered(r.kind),
        })
        .collect();

    // Anomalies: keep only RFC-audit findings (stable rule id), map lane→role,
    // clear the timing/wire-bearing detail, sort for determinism.
    let mut anomalies: Vec<Anomaly> = doc
        .anomalies
        .iter()
        .filter(|a| is_rfc_rule(&a.check))
        .map(|a| Anomaly {
            check: a.check.clone(),
            detail: String::new(),
            lane: a.lane.as_deref().map(&resolve),
            endpoint: None,
            advisory: a.advisory,
        })
        .collect();
    anomalies.sort_by(|a, b| a.check.cmp(&b.check).then(a.lane.cmp(&b.lane)));

    SeqDoc {
        title: doc.title.clone(),
        // Description carries the per-lane banner (resolved binding / cell name)
        // and is dropped: it is provenance, not plan.
        description: None,
        passed: doc.passed,
        lanes,
        rows,
        anomalies,
        // Wall/virtual timing is excluded — the normalized axis is the ordinal.
        epoch_base_ms: None,
    }
}

/// A lane id fallback role when no map entry exists: the sub-lane suffix
/// (`ip:port#name` → `name`), else the id with any address stripped.
fn role_of_lane_id(id: &str) -> String {
    if let Some((_, name)) = id.split_once('#') {
        return name.to_string();
    }
    if looks_like_addr(id) {
        return "ua".to_string();
    }
    id.to_string()
}

/// A retransmission row (collapsed onto its first occurrence).
fn is_retransmit(label: &str) -> bool {
    RETRANSMIT_MARKERS.iter().any(|m| label.contains(m))
}

/// A SIP row's label reduced to its method or status token only (the leading
/// token) — the R-URI/target (which carries a host:port) and the projector's
/// receive-note decorations are dropped.
fn normalize_label(label: &str) -> String {
    label.split_whitespace().next().unwrap_or("").to_string()
}

/// Force a message row's per-attempt delivery flag to `true` (delivery is a
/// transport fact); lifecycle rows never reach here.
fn force_delivered(kind: RowKind) -> RowKind {
    match kind {
        RowKind::Sip { .. } => RowKind::Sip { delivered: true },
        RowKind::Repl { .. } => RowKind::Repl { delivered: true },
        RowKind::Lifecycle => RowKind::Lifecycle,
    }
}

/// Whether an anomaly's `check` id names an RFC-audit rule (the only
/// normalization-stable anomaly class).
fn is_rfc_rule(check: &str) -> bool {
    check.starts_with("rfc")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LaneKind;

    fn sip_row(seq: u64, at_ms: i64, from: &str, to: &str, label: &str) -> SeqRow {
        SeqRow {
            at_ms,
            seq,
            from: from.into(),
            to: Some(to.into()),
            label: label.into(),
            detail: Some("INVITE sip:bob SIP/2.0\r\nwire...".into()),
            conn: Some("call-id-xyz@10.0.0.9".into()),
            kind: RowKind::Sip { delivered: true },
        }
    }

    fn doc() -> SeqDoc {
        SeqDoc {
            title: "basic_call".into(),
            description: Some("binding: case=t from=… to=…".into()),
            passed: true,
            lanes: vec![
                Lane::new("10.0.0.1:5060", "alice (10.0.0.1:5060)", LaneKind::Ua),
                Lane::new("10.0.0.9:5070#bob", "bob", LaneKind::Ua).with_group("10.0.0.9:5070"),
            ],
            rows: vec![
                sip_row(2, 200, "10.0.0.1:5060", "10.0.0.9:5070#bob", "INVITE sip:bob@10.0.0.9:5070"),
                sip_row(1, 100, "10.0.0.9:5070#bob", "10.0.0.1:5060", "200 OK"),
            ],
            anomalies: vec![
                Anomaly {
                    check: "rfc3261.cseqInDialogOrder".into(),
                    detail: "cseq=2 out of order at :5070".into(),
                    lane: Some("10.0.0.9:5070#bob".into()),
                    endpoint: Some("bob".into()),
                    advisory: Some(false),
                },
                Anomaly {
                    check: "queueLeak".into(),
                    detail: "1 in flight".into(),
                    lane: None,
                    endpoint: None,
                    advisory: Some(true),
                },
            ],
            epoch_base_ms: Some(1_782_802_100_000),
        }
    }

    fn norm(d: &SeqDoc) -> SeqDoc {
        normalize(d, &role_map_from_lanes(&d.lanes))
    }

    #[test]
    fn determinism_same_doc_serializes_identically() {
        let d = doc();
        let a = serde_json::to_string(&norm(&d)).unwrap();
        let b = serde_json::to_string(&norm(&d)).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn idempotence() {
        let d = doc();
        let n1 = norm(&d);
        let n2 = norm(&n1);
        assert_eq!(n1, n2);
        // And serializes identically too.
        assert_eq!(
            serde_json::to_string(&n1).unwrap(),
            serde_json::to_string(&n2).unwrap()
        );
    }

    #[test]
    fn retransmit_row_collapses_onto_first_occurrence() {
        let mut d = doc();
        // A timer re-emit of the INVITE (the projector's ⟳ marker) after the
        // first occurrence — must not survive.
        d.rows.push(sip_row(3, 260, "10.0.0.1:5060", "10.0.0.9:5070#bob", "INVITE sip:bob@x \u{21bb} [re-emit: timer]"));
        let n = norm(&d);
        let invites = n.rows.iter().filter(|r| r.label == "INVITE").count();
        assert_eq!(invites, 1, "the retransmit collapses onto the first INVITE");
    }

    #[test]
    fn fake_lane_doc_has_no_address_in_the_serialized_form() {
        let n = norm(&doc());
        let json = serde_json::to_string(&n).unwrap();
        // No ip:port, no sub-lane key, no ephemeral port anywhere.
        assert!(!json.contains("10.0.0.1"), "no address leaks:\n{json}");
        assert!(!json.contains("5070"), "no port leaks:\n{json}");
        assert!(!json.contains('#'), "no sub-lane key leaks:\n{json}");
        // Lane ids ARE role names.
        assert!(n.lanes.iter().any(|l| l.id == "alice"));
        assert!(n.lanes.iter().any(|l| l.id == "bob"));
        // Rows reference roles; labels are method/status only.
        assert!(n.rows.iter().all(|r| r.from == "alice" || r.from == "bob"));
        assert!(n.rows.iter().any(|r| r.label == "INVITE"));
        assert!(n.rows.iter().any(|r| r.label == "200"));
        // The stable RFC anomaly survives (lane mapped to role, detail cleared);
        // the structural one is gone.
        assert_eq!(n.anomalies.len(), 1);
        assert_eq!(n.anomalies[0].check, "rfc3261.cseqInDialogOrder");
        assert_eq!(n.anomalies[0].lane.as_deref(), Some("bob"));
        assert!(n.anomalies[0].detail.is_empty());
    }

    #[test]
    fn causal_order_rebases_at_ms_and_seq_onto_the_ordinal() {
        let n = norm(&doc());
        // Sorted by original seq: the 200 (seq 1) precedes the INVITE (seq 2).
        assert_eq!(n.rows[0].label, "200");
        assert_eq!(n.rows[1].label, "INVITE");
        assert_eq!((n.rows[0].at_ms, n.rows[0].seq), (0, 0));
        assert_eq!((n.rows[1].at_ms, n.rows[1].seq), (1, 1));
        assert_eq!(n.epoch_base_ms, None);
    }
}
