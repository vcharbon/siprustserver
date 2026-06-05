//! Focused replication-exchange report (ADR-0006 recording-first).
//!
//! ## Why a focused renderer instead of reusing `scenario-harness`
//! The `scenario-harness` renderers ([`report::svg`]/`text`/`html`) consume a
//! SIP-specific recording: `RecordedSipEntry` + `Lane`s drained from the
//! `layer-harness` `Recorder`, with wire text formatted by `sip-net`. Projecting
//! a [`CapturedFrame`] (a decoded *replication* `Frame` with `(from,to,dir)`
//! endpoints) onto them would mean fabricating synthetic SIP entries — the model
//! does not fit (there is no SIP method/CSeq/branch; the "message" is a
//! `PullRequest`/`Data`/`Noop`). So per the slice brief we build a FOCUSED
//! replication renderer here: a text sequence diagram (the bar) plus a mermaid
//! `sequenceDiagram` (the bonus), both projecting the captured frame exchange
//! with lanes = node ordinals and timestamps, and crash/reboot/partition markers
//! injected into the timeline. It reads ONLY the recording (captured frames +
//! the marker log) — never live engine state — exactly as ADR-0006 intends.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use repl_net::transport::{CapturedFrame, Direction};
use repl_net::{Frame, Op, Partition, PullMode};

/// A non-frame timeline event injected by the harness (crash / reboot /
/// partition / heal / put / delete), stamped with the recording clock.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Marker {
    /// Recording timestamp (ms) from the injected `Clock`.
    pub at_ms: i64,
    /// Global recording-order sequence drawn from the shared capture sequencer
    /// at the instant the marker was appended (crash/reboot/partition/heal/…).
    /// `0` for a standalone repl recording with no shared sequencer. When the
    /// failover harness threads the SIP recorder's sequencer in, this orders the
    /// marker against SIP messages and replication frames in TRUE append order,
    /// so a reboot marker appended just before the bootstrap pull it triggers
    /// sorts first even though both share the same millisecond.
    pub seq: u64,
    /// The node ordinal this event concerns (the actor / `from` lane).
    pub node: String,
    /// Optional second ordinal (e.g. the partition peer) for two-node events.
    pub peer: Option<String>,
    /// A short human-readable kind: `crash`, `reboot`, `partition`, `heal`,
    /// `cut`, `put`, `delete`, …
    pub kind: String,
    /// Free-form detail (e.g. the callRef, the new incarnation gen).
    pub detail: String,
}

/// One ordered line of the projected exchange — a frame or a marker.
enum TimelineRow<'a> {
    Frame(&'a CapturedFrame),
    Marker(&'a Marker),
}

fn row_at(row: &TimelineRow) -> i64 {
    match row {
        TimelineRow::Frame(f) => f.at_ms,
        TimelineRow::Marker(m) => m.at_ms,
    }
}

/// The recording snapshot a report renders from: the captured frames + the
/// injected markers + the addr→ordinal lane map. Built by the cluster.
pub struct ReplReport {
    /// Every captured replication frame, in append order.
    pub frames: Vec<CapturedFrame>,
    /// Injected crash/reboot/partition/put/delete markers, in append order.
    pub markers: Vec<Marker>,
    /// `SocketAddr → node ordinal` so lanes read as node names, not ports.
    pub lanes: BTreeMap<SocketAddr, String>,
}

impl ReplReport {
    /// The full addr→ordinal map: the static listen lanes PLUS the ephemeral
    /// client addresses recovered from each `PullRequest`'s `caller` field (a
    /// puller connects from an ephemeral local addr; its first frame names the
    /// caller ordinal, so we can label that addr's lane as the caller node).
    fn lane_map(&self) -> BTreeMap<SocketAddr, String> {
        let mut map = self.lanes.clone();
        for f in &self.frames {
            if let Frame::PullRequest { caller, .. } = &f.frame {
                // The PullRequest is sent FROM the client's ephemeral local addr
                // (dir=Sent) or arrives at the server (dir=Received, from=client).
                let client_addr = match f.dir {
                    Direction::Sent => f.from,
                    Direction::Received => f.from,
                };
                map.entry(client_addr).or_insert_with(|| caller.clone());
            }
        }
        map
    }

    /// Resolve an addr to its node ordinal (lane label), falling back to the
    /// addr string for an unknown endpoint.
    fn lane(&self, addr: SocketAddr) -> String {
        self.lane_map()
            .get(&addr)
            .cloned()
            .unwrap_or_else(|| addr.to_string())
    }

    /// The distinct node lanes, sorted by ordinal (deterministic order).
    pub fn node_lanes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.lanes.values().cloned().collect();
        v.sort();
        v.dedup();
        v
    }

    /// `true` if any captured frame matches `pred` (test introspection).
    pub fn any_frame(&self, pred: impl Fn(&Frame) -> bool) -> bool {
        self.frames.iter().any(|c| pred(&c.frame))
    }

    /// Count of captured frames.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Merge frames + markers into one timestamp-ordered timeline. We render
    /// only `Sent` frames (a `Sent`/`Received` pair is the same logical
    /// message; `Sent` carries the originating lane) so the diagram reads as one
    /// arrow per message rather than doubled.
    fn timeline(&self) -> Vec<TimelineRow<'_>> {
        let mut rows: Vec<TimelineRow> = Vec::new();
        for f in &self.frames {
            if f.dir == Direction::Sent {
                rows.push(TimelineRow::Frame(f));
            }
        }
        for m in &self.markers {
            rows.push(TimelineRow::Marker(m));
        }
        // Stable sort by timestamp: preserves append order within one tick so a
        // marker injected before a frame at the same ms reads first.
        rows.sort_by_key(|r| row_at(r));
        rows
    }

    /// Project this recording into a neutral [`seq_report::SeqDoc`] — the
    /// replication-only view of the SHARED unified renderer (see `seq-report`'s
    /// crate docs). Lanes are the node ordinals; replication frames are `Repl`
    /// rows; crash/reboot/partition/put/delete markers are `Lifecycle` bands.
    ///
    /// Only `Sent` frames become rows (a `Sent`/`Received` pair is the same
    /// logical message; `Sent` carries the originating lane), matching the
    /// historic timeline so the diagram reads as one arrow per message.
    pub fn to_seq_doc(&self) -> seq_report::SeqDoc {
        use seq_report::{Lane, LaneKind, RowKind, SeqRow};

        let lane_map = self.lane_map();
        let lanes: Vec<Lane> = self
            .node_lanes()
            .into_iter()
            .map(|ord| Lane::new(ord.clone(), ord, LaneKind::Node))
            .collect();

        // The renderer sorts by the global `seq` (recording order). This
        // standalone repl-only view has no shared SIP sequencer, so we assign
        // `seq` ourselves over the merged frame+marker timeline — using the same
        // stable `at_ms`-ordered interleave [`timeline`](Self::timeline) builds —
        // so a marker injected just before a frame at the same ms still reads
        // first. (Frames and markers must NOT be assigned seq in two separate
        // blocks: that would group all frames before all markers regardless of
        // time once the renderer sorts by seq.)
        let mut rows: Vec<SeqRow> = Vec::new();
        for (seq, row) in self.timeline().into_iter().enumerate() {
            let seq = seq as u64 + 1;
            match row {
                TimelineRow::Frame(f) => {
                    let from =
                        lane_map.get(&f.from).cloned().unwrap_or_else(|| f.from.to_string());
                    let to = lane_map.get(&f.to).cloned().unwrap_or_else(|| f.to.to_string());
                    rows.push(SeqRow {
                        at_ms: f.at_ms,
                        seq,
                        from,
                        to: Some(to),
                        label: frame_summary(&f.frame),
                        detail: None,
                        kind: RowKind::Repl { delivered: true },
                    });
                }
                TimelineRow::Marker(m) => {
                    rows.push(SeqRow {
                        at_ms: m.at_ms,
                        seq,
                        from: m.node.clone(),
                        to: None,
                        label: marker_label(m),
                        detail: (!m.detail.is_empty()).then(|| m.detail.clone()),
                        kind: RowKind::Lifecycle,
                    });
                }
            }
        }

        seq_report::SeqDoc {
            title: "Replication exchange (recording-first; ADR-0006)".to_string(),
            description: None,
            passed: true,
            lanes,
            rows,
            anomalies: Vec::new(),
        }
    }

    /// Render a readable TEXT sequence diagram of the replication exchange via
    /// the SHARED unified renderer (over [`to_seq_doc`](Self::to_seq_doc)). Each
    /// frame is a `FROM -> TO  <frame-summary>` line; each marker a centred
    /// `=== marker ===` band.
    pub fn render_text(&self) -> String {
        seq_report::render_global_txt(&self.to_seq_doc())
    }

    /// Render the replication exchange as a self-contained HTML sequence diagram
    /// via the SHARED unified renderer (cheap — same `SeqDoc` as the text view).
    pub fn render_html(&self) -> String {
        seq_report::render_html(&self.to_seq_doc())
    }

    /// Render a mermaid `sequenceDiagram` (the bonus): participants = node
    /// lanes; frames = arrows; markers = `Note` bands. Paste into any mermaid
    /// renderer for a graphical sequence.
    pub fn render_mermaid(&self) -> String {
        let mut out = String::new();
        out.push_str("sequenceDiagram\n");
        for lane in self.node_lanes() {
            out.push_str(&format!("    participant {lane}\n"));
        }
        for row in self.timeline() {
            match row {
                TimelineRow::Frame(f) => {
                    let from = self.lane(f.from);
                    let to = self.lane(f.to);
                    out.push_str(&format!(
                        "    {}->>{}: t{} {}\n",
                        from,
                        to,
                        f.at_ms,
                        frame_summary(&f.frame),
                    ));
                }
                TimelineRow::Marker(m) => {
                    let lane = &m.node;
                    out.push_str(&format!("    Note over {}: t{} {}\n", lane, m.at_ms, marker_label(m)));
                }
            }
        }
        out
    }
}

/// A compact one-line summary of a replication frame for the diagram.
pub fn frame_summary(frame: &Frame) -> String {
    match frame {
        Frame::PullRequest { mode, since, chunk, caller, .. } => {
            let m = match mode {
                PullMode::Replog => "Replog",
                PullMode::Bootstrap => "Bootstrap",
            };
            format!(
                "PullRequest[{m}] caller={caller} since=({},{}) chunk={chunk}",
                since.gen, since.counter
            )
        }
        Frame::Ack { caller, up_to } => {
            format!("Ack caller={caller} up_to=({},{})", up_to.gen, up_to.counter)
        }
        Frame::Data { at, op, partition, call_ref, call_gen, call_bgen, body, .. } => {
            let o = match op {
                Op::Create => "Create",
                Op::Update => "Update",
                Op::Delete => "Delete",
            };
            let p = match partition {
                Partition::Pri => "pri",
                Partition::Bak => "bak",
            };
            let blen = body.as_ref().map(|b| b.len()).unwrap_or(0);
            format!(
                "Data[{o}/{p}] {call_ref} cv=({call_gen},{call_bgen}) at=({},{}) body={blen}B",
                at.gen, at.counter
            )
        }
        Frame::Noop { at } => format!("Noop at=({},{})", at.gen, at.counter),
        Frame::ResetToBootstrap { reason } => format!("ResetToBootstrap reason={reason}"),
    }
}

fn marker_label(m: &Marker) -> String {
    match &m.peer {
        Some(peer) if !m.detail.is_empty() => {
            format!("{} {}<->{} {}", m.kind, m.node, peer, m.detail)
        }
        Some(peer) => format!("{} {}<->{}", m.kind, m.node, peer),
        None if !m.detail.is_empty() => format!("{} {} {}", m.kind, m.node, m.detail),
        None => format!("{} {}", m.kind, m.node),
    }
}
