//! The unified-timeline COMBINER — the failover harness's projector onto the
//! SHARED [`seq_report`] renderer.
//!
//! This is the crux of the unified report (see `seq-report`'s crate docs): it
//! merges the SIP plane, the lifecycle plane, and the replication plane into ONE
//! [`seq_report::SeqDoc`] on a SHARED lane axis so a human can read the
//! interleaving — did the crash land before or after the backup absorbed the
//! in-dialog request? did the reclaim's handback reach the survivor before the
//! next OPTIONS? — which is impossible when each plane is rendered separately.
//!
//! ## The unified lane axis
//! The columns are, in order: `alice, proxy, b1, b2, bob`. The collapse that
//! makes the diagram legible: a worker's repl node (its `9400+i` repl addr) maps
//! onto the SAME column as its SIP traffic and its lifecycle markers, so `b1`'s
//! column shows its SIP messages AND its repl frames AND its crash/reboot bands
//! together. The mapping is:
//!   - SIP rows: `RecordedSipEntry::{from,to}` (SIP addr) → column by SIP addr,
//!   - replication rows: repl addr → node ordinal → that ordinal's column,
//!   - lifecycle markers: `Marker::node` (ordinal) → that ordinal's column.
//!
//! Each plane keeps its own [`seq_report::RowKind`] so the renderer styles them
//! distinctly (SIP solid, replication dashed, lifecycle as a band).

use std::collections::BTreeMap;
use std::net::SocketAddr;

use ha_harness::{frame_summary, ReplReport};
use layer_harness::{NetworkTag, RecordedScenario};
use repl_net::transport::Direction;
use seq_report::{Anomaly, Lane, LaneKind, RowKind, SeqDoc, SeqRow};
use sip_net::RecordedSipEntry;

use scenario_harness::report::wire::{facets, wire_text};

/// One worker's identity across the planes: its ordinal (the column key + the
/// marker `node` + the repl lane label), its SIP wire address, and its repl
/// listen address. Built by the harness from its own `repl_addrs` + the SIP
/// bindings.
#[derive(Clone, Debug)]
pub struct WorkerAxis {
    /// Cluster ordinal (`b1`/`b2`) — the shared column id.
    pub ordinal: String,
    /// The worker's SIP wire address (e.g. `127.0.0.1:5091`).
    pub sip_addr: SocketAddr,
    /// The worker's repl listen address (e.g. `127.0.0.1:9400`).
    pub repl_addr: SocketAddr,
}

/// Build the ONE unified [`SeqDoc`] for a failover run.
///
/// `title`/`description`/`passed` head the document. `sip_entries` + `scenario`
/// are the SIP recording (lanes + anomalies + wire trace). `repl` is the
/// replication recording (frames + lifecycle markers). `workers` ties the two
/// address spaces to the shared `b1`/`b2` columns.
pub fn combine_doc(
    title: &str,
    description: Option<&str>,
    passed: bool,
    sip_entries: &[RecordedSipEntry],
    scenario: &RecordedScenario,
    repl: &ReplReport,
    workers: &[WorkerAxis],
) -> SeqDoc {
    // --- 1. the shared lane axis -------------------------------------------
    // SIP addr → column id, and repl addr → column id (collapsing the repl node
    // onto its worker's SIP column).
    let mut sip_addr_col: BTreeMap<SocketAddr, String> = BTreeMap::new();
    let mut repl_addr_col: BTreeMap<SocketAddr, String> = BTreeMap::new();
    for w in workers {
        sip_addr_col.insert(w.sip_addr, w.ordinal.clone());
        repl_addr_col.insert(w.repl_addr, w.ordinal.clone());
    }

    let lanes = build_lanes(workers, scenario, &sip_addr_col);

    // Column resolver for a SIP address: a worker column if it is a worker SIP
    // addr, else the lane's own address string (alice/proxy/bob).
    let sip_col = |addr: SocketAddr| -> String {
        sip_addr_col
            .get(&addr)
            .cloned()
            .unwrap_or_else(|| addr.to_string())
    };

    // --- 2. the rows, one plane at a time, on ONE shared global seq ----------
    // Every source — SIP messages, repl frames, lifecycle markers — was stamped
    // from the SAME global recording-order sequencer (the SIP recorder's
    // `EventSequencer`) at the instant it was recorded, so the row `seq` values
    // are directly comparable ACROSS planes. The renderer then sorts by `seq`
    // alone (NOT `at_ms`), giving true append/causal order: a reboot marker
    // appended just before the bootstrap pull it triggers sorts first even when
    // both share the same paused-clock millisecond. `at_ms` is carried only for
    // the displayed `T+…` time label. (Issue 1.)
    let mut rows: Vec<SeqRow> = Vec::new();
    let base = sip_entries.iter().map(|e| e.sent_ms as i64).min().unwrap_or(0);

    // SIP plane.
    for e in sip_entries {
        let mut detail = String::new();
        if let Some(rcvd) = e.received_ms.filter(|r| *r != e.sent_ms) {
            detail.push_str(&format!(
                "┄ sent {} → rcvd {}\n",
                seq_report::format_relative(e.sent_ms as i64 - base),
                seq_report::format_relative(rcvd as i64 - base),
            ));
        }
        detail.push_str(&wire_text(&e.raw));
        rows.push(SeqRow {
            at_ms: e.sent_ms as i64,
            seq: e.seq,
            from: sip_col(e.from),
            to: Some(sip_col(e.to)),
            label: facets(&e.raw).label,
            detail: Some(detail),
            kind: RowKind::Sip {
                delivered: e.delivered,
            },
        });
    }

    // Replication plane (only `Sent` frames — one logical message per arrow).
    let repl_lane_map = repl_lane_map(repl, &repl_addr_col);
    for f in &repl.frames {
        if f.dir != Direction::Sent {
            continue;
        }
        let from = repl_lane_map
            .get(&f.from)
            .cloned()
            .unwrap_or_else(|| f.from.to_string());
        let to = repl_lane_map
            .get(&f.to)
            .cloned()
            .unwrap_or_else(|| f.to.to_string());
        rows.push(SeqRow {
            at_ms: f.at_ms,
            seq: f.seq,
            from,
            to: Some(to),
            label: frame_summary(&f.frame),
            detail: None,
            kind: RowKind::Repl { delivered: true },
        });
    }

    // Lifecycle plane (the crash/reboot/failover/partition/heal markers).
    for m in &repl.markers {
        let detail = match (&m.peer, m.detail.is_empty()) {
            (Some(p), false) => Some(format!("peer={p}; {}", m.detail)),
            (Some(p), true) => Some(format!("peer={p}")),
            (None, false) => Some(m.detail.clone()),
            (None, true) => None,
        };
        rows.push(SeqRow {
            at_ms: m.at_ms,
            seq: m.seq,
            from: m.node.clone(),
            to: None,
            label: marker_label(m),
            detail,
            kind: RowKind::Lifecycle,
        });
    }

    let anomalies = scenario
        .anomalies
        .iter()
        .map(|a| Anomaly {
            check: a.check.clone(),
            detail: a.detail.clone(),
            lane: a.bind_key.as_ref().map(|k| sip_col_str(k, &sip_addr_col)),
        })
        .collect();

    SeqDoc {
        title: title.to_string(),
        description: description.map(str::to_string),
        passed,
        lanes,
        rows,
        anomalies,
    }
}

/// Build the columns in the canonical `alice, proxy, b1, b2, bob` order: the
/// non-worker SIP lanes split around the workers by their natural sort, with the
/// worker columns inserted in declaration order between the UAs and the proxy.
fn build_lanes(
    workers: &[WorkerAxis],
    scenario: &RecordedScenario,
    sip_addr_col: &BTreeMap<SocketAddr, String>,
) -> Vec<Lane> {
    // Friendly names from the recording.
    let name_of = |addr: SocketAddr| -> Option<String> {
        scenario
            .lanes
            .iter()
            .find(|l| l.addr == addr)
            .and_then(|l| l.names.first().cloned())
    };

    // Partition the non-worker SIP lanes into UAs and the proxy/core SUT.
    let mut uas: Vec<(SocketAddr, String)> = Vec::new();
    let mut suts: Vec<(SocketAddr, String)> = Vec::new();
    for l in &scenario.lanes {
        if sip_addr_col.contains_key(&l.addr) {
            continue; // a worker — placed separately, collapsed onto its column
        }
        let name = l.names.first().cloned().unwrap_or_default();
        let is_proxy = matches!(l.network, NetworkTag::Core) || name == "proxy";
        if is_proxy {
            suts.push((l.addr, name));
        } else {
            uas.push((l.addr, name));
        }
    }
    // Stable deterministic order within each group.
    uas.sort_by(|a, b| a.0.cmp(&b.0));
    suts.sort_by(|a, b| a.0.cmp(&b.0));

    let label = |name: &str, id: &str| -> String {
        if name.is_empty() {
            id.to_string()
        } else {
            format!("{name} ({id})")
        }
    };

    let mut lanes: Vec<Lane> = Vec::new();
    // Caller-side UA first (alice).
    for (addr, name) in uas.iter().take(uas.len().saturating_sub(1)) {
        lanes.push(Lane::new(addr.to_string(), label(name, &addr.to_string()), LaneKind::Ua));
    }
    // The proxy SUT.
    for (addr, name) in &suts {
        lanes.push(Lane::new(addr.to_string(), label(name, &addr.to_string()), LaneKind::Sut));
    }
    // The worker nodes (b1, b2) in declaration order, keyed by ordinal so SIP +
    // repl + lifecycle all collapse here. The column caption pairs the ordinal
    // with its SIP wire address.
    for w in workers {
        lanes.push(Lane::new(
            w.ordinal.clone(),
            format!("{} ({})", w.ordinal, w.sip_addr),
            LaneKind::Node,
        ));
    }
    // Callee-side UA last (bob).
    if let Some((addr, name)) = uas.last() {
        lanes.push(Lane::new(
            addr.to_string(),
            label(name, &addr.to_string()),
            LaneKind::Ua,
        ));
    }

    let _ = name_of; // reserved for richer worker captions; SIP addr suffices now
    lanes
}

/// The repl addr → column map: the static worker repl addrs PLUS the ephemeral
/// puller addresses recovered from each `PullRequest`'s `caller` field (a puller
/// connects from an ephemeral local addr; its first frame names the caller
/// ordinal, so that addr collapses onto the caller's column).
fn repl_lane_map(
    repl: &ReplReport,
    repl_addr_col: &BTreeMap<SocketAddr, String>,
) -> BTreeMap<SocketAddr, String> {
    let mut map: BTreeMap<SocketAddr, String> = repl_addr_col.clone();
    for f in &repl.frames {
        if let repl_net::Frame::PullRequest { caller, .. } = &f.frame {
            map.entry(f.from).or_insert_with(|| caller.clone());
        }
    }
    map
}

/// Resolve a recorder `bind_key` (a SIP addr string) to its column id.
fn sip_col_str(key: &str, sip_addr_col: &BTreeMap<SocketAddr, String>) -> String {
    key.parse::<SocketAddr>()
        .ok()
        .and_then(|a| sip_addr_col.get(&a).cloned())
        .unwrap_or_else(|| key.to_string())
}

/// Compose a lifecycle marker caption (mirrors the ha-harness label form so the
/// repl-only and unified views read identically).
fn marker_label(m: &ha_harness::Marker) -> String {
    match &m.peer {
        Some(peer) if !m.detail.is_empty() => format!("{} {}<->{} {}", m.kind, m.node, peer, m.detail),
        Some(peer) => format!("{} {}<->{}", m.kind, m.node, peer),
        None if !m.detail.is_empty() => format!("{} {} {}", m.kind, m.node, m.detail),
        None => format!("{} {}", m.kind, m.node),
    }
}
