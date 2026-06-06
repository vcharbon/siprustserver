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
            conn: None,
            kind: RowKind::Sip {
                delivered: e.delivered,
            },
        });
    }

    // Replication plane (only `Sent` frames — one logical message per arrow).
    // A frame is `delivered` iff the peer's recorder captured the matching
    // `Received` (see [`delivered_sent`]); a frame the sender emitted into a dead
    // or cut connection (e.g. a survivor still streaming a reclaim response to a
    // just-crashed peer) has no `Received` twin and renders as LOST (a cross).
    let repl_lane_map = repl_lane_map(repl, &repl_addr_col);
    let delivered_sent = delivered_sent(&repl.frames);
    for (i, f) in repl.frames.iter().enumerate() {
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
            conn: Some(conn_label(f, &repl_addr_col)),
            kind: RowKind::Repl {
                delivered: delivered_sent.contains(&i),
            },
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
            conn: None,
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

/// Which `Sent` frames actually reached the peer — the set of indices into
/// `frames` of every `Sent` capture that has a matching `Received` twin.
///
/// A `Sent` is captured on the sender's `send()` (before the wire moves it); the
/// peer's `recv()` captures the `Received` only if the byte actually arrived. So
/// a `Sent` with no `Received` twin was **lost in transit** — the classic case
/// is a survivor still streaming a reclaim response down a connection whose far
/// end just crashed (the frame is faithfully recorded as *attempted*, never
/// applied). The two captures sit at opposite endpoints, so a `Sent (from=A,
/// to=B)` is twinned by a `Received (from=B, to=A)` (orientation swapped, since
/// each side records its own local addr as `from`), same `Frame`, captured at or
/// after the send.
///
/// Pairing is 1:1 and duplicate-safe: the SAME `(A→B, frame)` may be sent twice
/// (e.g. dropped pre-reboot, then re-sent and delivered). We walk `Received` in
/// time order and claim, for each, the most-recent unconsumed matching `Sent` at
/// or before it — so a later delivery can never "rescue" an earlier dropped send.
fn delivered_sent(frames: &[repl_net::transport::CapturedFrame]) -> std::collections::HashSet<usize> {
    let sent: Vec<(usize, &repl_net::transport::CapturedFrame)> = frames
        .iter()
        .enumerate()
        .filter(|(_, f)| f.dir == Direction::Sent)
        .collect();
    let mut received: Vec<&repl_net::transport::CapturedFrame> =
        frames.iter().filter(|f| f.dir == Direction::Received).collect();
    received.sort_by_key(|f| f.at_ms);

    let mut consumed = vec![false; sent.len()];
    let mut delivered = std::collections::HashSet::new();
    for r in received {
        // The most-recent unconsumed Sent twin at or before this receipt.
        let mut best: Option<usize> = None;
        for (pos, (_, s)) in sent.iter().enumerate() {
            if consumed[pos]
                || s.frame != r.frame
                || s.from != r.to
                || s.to != r.from
                || s.at_ms > r.at_ms
            {
                continue;
            }
            if best.is_none_or(|b| s.at_ms >= sent[b].1.at_ms) {
                best = Some(pos);
            }
        }
        if let Some(pos) = best {
            consumed[pos] = true;
            delivered.insert(sent[pos].0);
        }
    }
    delivered
}

/// The disambiguating socket identity of the connection a repl frame rode — the
/// ephemeral (puller) endpoint, i.e. the one that is NOT a worker's fixed repl
/// listen address. Two flows to the same node use two distinct ephemeral
/// sockets, and a node that crashes + reconnects gets a fresh one; rendering
/// each `conn` in its own color makes a frame "lost to b2" legible as riding a
/// DIFFERENT (defunct) socket than the live connection collapsed on that lane.
fn conn_label(
    f: &repl_net::transport::CapturedFrame,
    repl_addr_col: &BTreeMap<SocketAddr, String>,
) -> String {
    // The listen end is in `repl_addr_col`; the other end is the ephemeral one.
    let ephemeral = if repl_addr_col.contains_key(&f.from) { f.to } else { f.from };
    format!(":{}", ephemeral.port())
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
    uas.sort_by_key(|a| a.0);
    suts.sort_by_key(|a| a.0);

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

#[cfg(test)]
mod tests {
    use super::*;
    use repl_net::transport::CapturedFrame;
    use repl_net::{Frame, Watermark};

    fn addr(port: u16) -> SocketAddr {
        format!("127.0.0.1:{port}").parse().unwrap()
    }

    /// Capture a frame `a → b` at `at_ms`. `dir` records on whichever endpoint
    /// observed it: a `Sent` records on the sender (`from=a`), a `Received` on the
    /// receiver — and because each side records its OWN local addr as `from`, a
    /// receipt of `a → b` is captured with `from=b, to=a` (orientation swapped),
    /// exactly as the real recorder does. The helper takes the LOGICAL direction
    /// (`a` is always the sender) and swaps for `Received` so tests read naturally.
    fn cap(at_ms: i64, a: u16, b: u16, dir: Direction, frame: Frame) -> CapturedFrame {
        let (from, to) = match dir {
            Direction::Sent => (addr(a), addr(b)),
            Direction::Received => (addr(b), addr(a)),
        };
        CapturedFrame { at_ms, seq: 0, from, to, dir, frame }
    }

    fn noop() -> Frame {
        Frame::Noop { at: Watermark::new(1, 2) }
    }

    #[test]
    fn sent_with_a_received_twin_is_delivered_without_one_is_lost() {
        // b1(9401) sends two identical Noops to b2(9402): the first is delivered
        // (b2 recv'd it), the second is lost (b2 had crashed — no recv twin).
        let frames = vec![
            cap(100, 9401, 9402, Direction::Sent, noop()),
            cap(101, 9401, 9402, Direction::Received, noop()), // twin of the first
            cap(700, 9401, 9402, Direction::Sent, noop()),     // no twin → lost
        ];
        let delivered = delivered_sent(&frames);
        assert!(delivered.contains(&0), "first send had a recv twin");
        assert!(!delivered.contains(&2), "second send was lost (no recv twin)");
    }

    #[test]
    fn a_later_redelivery_does_not_rescue_an_earlier_dropped_send() {
        // The crux of the kill report: the SAME (b1→b2, frame) is sent at t=170
        // (lost, peer dead) then re-sent at t=230 after reboot (delivered). The
        // single receipt at t=231 must pair with the t=230 send — NOT retroactively
        // mark the t=170 drop as delivered.
        let frames = vec![
            cap(170, 9401, 9402, Direction::Sent, noop()),     // dropped (peer dead)
            cap(230, 9401, 9402, Direction::Sent, noop()),     // re-sent post-reboot
            cap(231, 9401, 9402, Direction::Received, noop()), // pairs with t=230
        ];
        let delivered = delivered_sent(&frames);
        assert!(!delivered.contains(&0), "the t=170 drop stays lost");
        assert!(delivered.contains(&1), "the t=230 re-send was delivered");
    }

    #[test]
    fn group_loss_count_equals_sent_minus_received() {
        // Six identical Noops sent, four received → exactly two marked lost
        // (which two among identical frames is immaterial; the COUNT is the claim).
        let mut frames = Vec::new();
        for i in 0..6 {
            frames.push(cap(100 + i * 10, 9401, 9402, Direction::Sent, noop()));
        }
        for i in 0..4 {
            frames.push(cap(105 + i * 10, 9401, 9402, Direction::Received, noop()));
        }
        let delivered = delivered_sent(&frames);
        let lost = (0..6).filter(|i| !delivered.contains(i)).count();
        assert_eq!(lost, 2, "6 sent − 4 received = 2 lost");
    }
}
