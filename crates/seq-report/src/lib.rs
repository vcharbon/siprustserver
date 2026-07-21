//! `seq-report` — the SHARED unified sequence-diagram renderer.
//!
//! Every test harness in this workspace records events on three logical planes
//! that share ONE virtual clock (`Clock::test_at(0)` / `tokio::time`), so their
//! timestamps are directly comparable:
//!
//! 1. the **SIP** plane (alice / proxy / b2bua workers / bob — request/response
//!    datagrams crossing the simulated signaling fabric),
//! 2. the **lifecycle** plane (operator/chaos events: stop a worker, restart it,
//!    crash / reboot / failover / partition / heal),
//! 3. the **replication** plane (the HA changelog frames flowing between worker
//!    nodes: `PullRequest` / `Data` / `Noop` / …).
//!
//! Historically each plane had its own renderer (scenario-harness's SIP
//! HTML/text, ha-harness's replication text/mermaid) and the failover harness
//! merely concatenated them. That made it impossible to *read the interleaving*
//! — which is exactly what you need to understand a failover bug: did the crash
//! land before or after the backup absorbed the in-dialog request? did the
//! reclaim's `Deactivate` reach the survivor before the next OPTIONS?
//!
//! This crate renders ALL THREE planes as one sequence, sorted by `(at_ms, seq)`,
//! on a shared set of lanes. It is deliberately a leaf crate with **zero**
//! workspace dependencies and (by default) no external crates: it defines a
//! small set of neutral input types ([`SeqDoc`] of [`SeqRow`]s + [`Lane`]s) and
//! two pure functions over them, [`render_html`] and [`render_global_txt`]. Each
//! harness owns a tiny "projector" that converts its own recording types into a
//! `SeqDoc` and calls in here. Keeping the renderer free of `sip-net`,
//! `repl-net`, `ha-harness`, `scenario-harness`, `b2bua`, … is what lets every
//! plane reuse it without a dependency cycle.
//!
//! ## The unified lane axis (the crux)
//! A [`SeqDoc`] declares an ordered list of [`Lane`]s (the diagram columns). A
//! [`SeqRow`] references lanes by [`Lane::id`]. The projector is responsible for
//! COLLAPSING planes onto shared columns where they describe the same actor: in
//! the failover view the replication endpoints `b1`/`b2` map onto the SAME
//! columns as those workers' SIP traffic and lifecycle markers, so one column
//! shows a worker's SIP messages AND its repl frames AND its crash/reboot bands.
//! That is what makes the interleaving legible.
//!
//! ## Row kinds
//! - [`RowKind::Sip`] / [`RowKind::Repl`] are point-to-point arrows
//!   (`from` → `to`); the two planes are styled differently (colour in HTML, a
//!   plane tag in text) so a human can tell them apart at a glance.
//! - [`RowKind::Lifecycle`] is a full-width BAND across all lanes (`to == None`),
//!   a centred annotation in time order (e.g. `crash b1`, `reboot b1`).

mod html;
mod normalize;
mod text;

pub use html::{render_embed, render_html, render_svg};
pub use normalize::{normalize, role_map_from_lanes};
pub use text::render_global_txt;

/// What an actor lane represents — drives only its styling/label decoration, not
/// its position (the projector fixes column order via the `lanes` vector).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LaneKind {
    /// A user agent / external endpoint (alice, bob).
    Ua,
    /// A system-under-test core on the internal fabric (the proxy).
    Sut,
    /// A cluster node that carries BOTH SIP and replication traffic plus
    /// lifecycle markers (a b2bua worker, e.g. `b1`/`b2`).
    Node,
}

/// One diagram column. `id` is the stable key rows reference; `label` is the
/// human caption shown at the column head.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Lane {
    /// Stable identity referenced by [`SeqRow::from`] / [`SeqRow::to`].
    pub id: String,
    /// Column caption (e.g. `b1 (127.0.0.1:5091)`).
    pub label: String,
    /// What the lane represents (styling only).
    pub kind: LaneKind,
    /// Shared-resource header this lane belongs under (e.g. the `ip:port` of a
    /// shared mux socket whose LOGICAL endpoints each get their own sub-lane —
    /// newkahneed-036 ask C). Consecutive lanes with the same `group` render
    /// one bracketing header above their individual captions. `None` (the
    /// default, and every pre-existing doc) renders exactly as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl Lane {
    /// Convenience constructor.
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: LaneKind) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind,
            group: None,
        }
    }

    /// Attach the shared-resource group header (see [`Lane::group`]).
    pub fn with_group(mut self, group: impl Into<String>) -> Self {
        self.group = Some(group.into());
        self
    }
}

/// Which plane a row belongs to. `delivered` carries through for the message
/// planes so an undelivered (lost / unbound) datagram or frame is flagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum RowKind {
    /// A SIP request/response datagram.
    Sip {
        /// `false` when no matching receive was found (lost packet).
        delivered: bool,
    },
    /// A replication changelog frame.
    Repl {
        /// `false` when the frame was not observed delivered.
        delivered: bool,
    },
    /// An operator/chaos event — rendered as a full-width band, not an arrow.
    Lifecycle,
}

/// One time-ordered event on the unified timeline. For the message planes it is
/// a point-to-point arrow `from → to`; for [`RowKind::Lifecycle`] it is a band
/// (`to == None`) optionally anchored at the `from` lane.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeqRow {
    /// Virtual-clock timestamp (ms). The primary sort key.
    pub at_ms: i64,
    /// Capture-order tiebreaker within one `at_ms`.
    pub seq: u64,
    /// Originating lane id (the actor). For a lifecycle band this is the node
    /// the event concerns, used to anchor it visually.
    pub from: String,
    /// Destination lane id, or `None` for a lifecycle band.
    pub to: Option<String>,
    /// Short caption (e.g. `INVITE sip:bob@…`, `Data[Create/bak] …`, `crash b1`).
    pub label: String,
    /// Optional expandable detail (e.g. the full wire text for a SIP message).
    pub detail: Option<String>,
    /// Optional connection/socket identity for a message row (e.g. the ephemeral
    /// replication socket `:40007`). Rows that share a `conn` are drawn in the
    /// same color, so distinct sockets are visually separable on a collapsed node
    /// lane: a reader can see that a frame "lost to b2" actually rode a DIFFERENT
    /// (now-defunct) socket than the live one. `None` rows use the plane's
    /// default color.
    pub conn: Option<String>,
    /// Which plane this row belongs to.
    pub kind: RowKind,
}

/// A recorded finding to surface alongside the diagram (e.g. an RFC audit hit).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Anomaly {
    /// The rule/invariant id (e.g. `rfc.cseqInDialogOrder`).
    pub check: String,
    /// Human-readable explanation.
    pub detail: String,
    /// The lane id the finding ties to, when one applies.
    pub lane: Option<String>,
    /// The display name of the endpoint behind `lane` (e.g. `lb`, `bob1`),
    /// when the projector could resolve one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `Some(true)` ⇒ informational only; `Some(false)` ⇒ a gating violation.
    /// `None` on documents written before severity was recorded (rendered as
    /// advisory).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub advisory: Option<bool>,
}

impl Anomaly {
    /// Severity for display: gating only when explicitly recorded as such.
    pub fn is_gating(&self) -> bool {
        self.advisory == Some(false)
    }
}

/// The complete neutral input to the renderers: a title, optional description,
/// pass/fail status, the ordered lanes (columns), the rows, and any anomalies.
///
/// Rows do not need to be pre-sorted — both renderers sort a copy by
/// `(at_ms, seq)` — but a projector may sort them for its own assertions.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SeqDoc {
    /// Diagram title (the scenario / cell name).
    pub title: String,
    /// Optional prose shown under the title.
    pub description: Option<String>,
    /// Overall pass/fail badge.
    pub passed: bool,
    /// The diagram columns, in display order.
    pub lanes: Vec<Lane>,
    /// The timeline rows (any order; sorted on render).
    pub rows: Vec<SeqRow>,
    /// Recorded findings to list under the diagram.
    pub anomalies: Vec<Anomaly>,
    /// Wall-clock epoch (ms) corresponding to the timeline base ([`base_ms`]).
    /// `Some` only when `at_ms` is real wall-clock-aligned time (the load driver's
    /// `Clock::system()` recording) — the renderers then show an ABSOLUTE UTC
    /// time alongside the relative `T+…` stamp, so a flow correlates to external
    /// events (e.g. a chaos kill instant). `None` for virtual-clock docs (the
    /// paused-runtime harness/failover views), which stay relative-only.
    ///
    /// `#[serde(default)]` so docs persisted before this field deserialize as
    /// `None` (relative-only), preserving the prior behaviour.
    #[serde(default)]
    pub epoch_base_ms: Option<i64>,
}

impl SeqDoc {
    /// Return the rows in canonical render order — shared by both renderers so
    /// HTML and text agree.
    ///
    /// The primary key is `seq`, the GLOBAL recording-order sequence: every
    /// source stamps its rows from one shared counter at the instant each event
    /// is recorded, so `seq` already encodes true append/causal order ACROSS
    /// planes. `at_ms` is only a display label and is NOT the cross-source
    /// tiebreaker — under a paused test clock many events collide on one
    /// millisecond, and ordering those by `at_ms` would mis-order them (e.g. a
    /// reboot marker would float after the bootstrap pull it caused). `at_ms`
    /// breaks a `seq` tie only as a last resort (two rows that genuinely share a
    /// sequence number — which a single shared sequencer never produces).
    pub(crate) fn sorted_rows(&self) -> Vec<&SeqRow> {
        let mut rows: Vec<&SeqRow> = self.rows.iter().collect();
        // Stable sort so equal-`seq` rows keep their input order as a final
        // backstop (a projector that left every `seq` at 0 falls back to its own
        // insertion order, which it is expected to build in timeline order).
        rows.sort_by(|a, b| a.seq.cmp(&b.seq).then(a.at_ms.cmp(&b.at_ms)));
        rows
    }

    /// The timeline base (earliest row timestamp), for relative `T+…` stamps.
    pub(crate) fn base_ms(&self) -> i64 {
        self.rows.iter().map(|r| r.at_ms).min().unwrap_or(0)
    }

    /// The absolute wall-clock epoch (ms) for a row's `at_ms`, when this doc is
    /// wall-clock-aligned ([`epoch_base_ms`] set); `None` otherwise. Maps a
    /// timeline offset back onto real time: `epoch_base + (at_ms - base)`.
    pub(crate) fn epoch_at(&self, at_ms: i64) -> Option<i64> {
        self.epoch_base_ms.map(|e| e + (at_ms - self.base_ms()))
    }
}

/// Format a wall-clock epoch (ms) as an ABSOLUTE `HH:MM:SS.mmmZ` UTC time-of-day
/// — the absolute companion to [`format_relative`]. UTC (suffix `Z`) so it is
/// unambiguous regardless of the reader's timezone; dependency-free (no chrono).
/// Used by the renderers when a doc is wall-clock-aligned so a callflow's frames
/// (and any chaos-marker band) carry a real time that correlates to external
/// events. Date is omitted — a single callflow never spans midnight.
pub fn format_epoch_utc(epoch_ms: i64) -> String {
    let ms = epoch_ms.rem_euclid(1000);
    let secs_of_day = epoch_ms.div_euclid(1000).rem_euclid(86_400);
    let h = secs_of_day / 3600;
    let m = (secs_of_day % 3600) / 60;
    let s = secs_of_day % 60;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

/// Format a virtual-clock offset (ms, relative to the first row) as `T+SEC.mmms`
/// — e.g. `T+0.015s`, `T+1m02.345s`. Shared by both renderers (and a port of
/// the SIP report's `formatRelativeTimestamp`) so timestamps read identically.
pub fn format_relative(ms: i64) -> String {
    let ms = ms.max(0);
    let total_sec = ms / 1000;
    let millis = ms % 1000;
    let min = total_sec / 60;
    let sec = total_sec % 60;
    if min > 0 {
        format!("T+{min}m{sec:02}.{millis:03}s")
    } else {
        format!("T+{sec}.{millis:03}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lane(id: &str, kind: LaneKind) -> Lane {
        Lane::new(id, format!("{id} (lane)"), kind)
    }

    /// A small mixed-plane doc: alice→b1 INVITE, a crash band, then a b1→b2
    /// repl Data, out of input order to prove the renderer sorts.
    fn mixed_doc() -> SeqDoc {
        SeqDoc {
            title: "mixed".into(),
            description: Some("three planes".into()),
            passed: true,
            lanes: vec![
                lane("alice", LaneKind::Ua),
                lane("b1", LaneKind::Node),
                lane("b2", LaneKind::Node),
            ],
            rows: vec![
                SeqRow {
                    at_ms: 200,
                    seq: 3,
                    from: "b1".into(),
                    to: Some("b2".into()),
                    label: "Data[Create/bak] x".into(),
                    detail: None,
                    conn: None,
                    kind: RowKind::Repl { delivered: true },
                },
                SeqRow {
                    at_ms: 0,
                    seq: 1,
                    from: "alice".into(),
                    to: Some("b1".into()),
                    label: "INVITE sip:bob".into(),
                    detail: Some("INVITE sip:bob SIP/2.0\r\n".into()),
                    conn: None,
                    kind: RowKind::Sip { delivered: true },
                },
                SeqRow {
                    at_ms: 100,
                    seq: 2,
                    from: "b1".into(),
                    to: None,
                    label: "crash b1".into(),
                    detail: None,
                    conn: None,
                    kind: RowKind::Lifecycle,
                },
            ],
            anomalies: vec![],
            epoch_base_ms: None,
        }
    }

    /// Issue 1 regression: a lifecycle marker recorded at the SAME `at_ms` as a
    /// following message must render BEFORE it — order follows the global `seq`
    /// (true recording order), NOT `at_ms`. This is the keepalive-cell symptom:
    /// the `reboot` band must precede the `PullRequest[Bootstrap]` it triggers
    /// even though both land on one paused-clock millisecond.
    #[test]
    fn lifecycle_row_sorts_before_a_same_ms_message_by_global_seq() {
        let doc = SeqDoc {
            title: "tie".into(),
            description: None,
            passed: true,
            lanes: vec![lane("b1", LaneKind::Node), lane("b2", LaneKind::Node)],
            // Both at the SAME at_ms; the marker was recorded FIRST (lower seq).
            // Pushed in REVERSE order to prove the renderer does not rely on input
            // order and orders by seq, not at_ms.
            rows: vec![
                SeqRow {
                    at_ms: 1_618,
                    seq: 8,
                    from: "b2".into(),
                    to: Some("b1".into()),
                    label: "PullRequest[Bootstrap] caller=b2".into(),
                    detail: None,
                    conn: None,
                    kind: RowKind::Repl { delivered: true },
                },
                SeqRow {
                    at_ms: 1_618,
                    seq: 7,
                    from: "b2".into(),
                    to: None,
                    label: "reboot b2 restart empty, higher gen".into(),
                    detail: None,
                    conn: None,
                    kind: RowKind::Lifecycle,
                },
            ],
            anomalies: vec![],
            epoch_base_ms: None,
        };
        let txt = render_global_txt(&doc);
        let reboot = txt.find("reboot b2").expect("reboot band present");
        let pull = txt.find("PullRequest[Bootstrap]").expect("pull present");
        assert!(
            reboot < pull,
            "lower-seq lifecycle row must sort before a same-at_ms message: {txt}"
        );
        // The HTML view must agree (both renderers share `sorted_rows`).
        let html = render_html(&doc);
        let h_reboot = html.find("reboot b2").expect("html reboot present");
        let h_pull = html.find("PullRequest[Bootstrap]").expect("html pull present");
        assert!(h_reboot < h_pull, "HTML honors the same global-seq order: {html}");
    }

    #[test]
    fn rows_render_in_time_order_not_input_order() {
        let doc = mixed_doc();
        let txt = render_global_txt(&doc);
        let invite = txt.find("INVITE").expect("INVITE present");
        let crash = txt.find("crash b1").expect("crash present");
        let data = txt.find("Data[Create/bak]").expect("Data present");
        assert!(invite < crash && crash < data, "sorted by (at_ms, seq): {txt}");
    }

    #[test]
    fn lifecycle_row_renders_as_a_band_in_both_outputs() {
        let doc = mixed_doc();
        let txt = render_global_txt(&doc);
        assert!(txt.contains("=== crash b1 ==="), "text band: {txt}");
        let html = render_html(&doc);
        // The band is a full-width SVG annotation (no payload, not a `.seq-msg`);
        // its label appears in the diagram.
        assert!(html.contains("crash b1"), "html band label present");
        // A lifecycle band is NOT a clickable message and has no payload block.
        assert!(
            !html.contains("⏻ crash b1") || !html.contains("id=\"evt-1\""),
            "the lifecycle band (ord 1) must not get an #evt-1 payload block"
        );
    }

    #[test]
    fn all_three_planes_appear_with_distinct_styling() {
        let doc = mixed_doc();
        let html = render_html(&doc);
        // SIP and REPL messages are clickable `.seq-msg` groups carrying a
        // plane class so they are visually distinguishable.
        assert!(html.contains("seq-msg seq-sip"), "sip message class");
        assert!(html.contains("seq-msg seq-repl"), "repl message class");
        // The lifecycle band renders (full-width, no class) — its label is shown.
        assert!(html.contains("crash b1"), "lifecycle band label");
        // The legend names all three planes.
        assert!(html.contains("class=\"legend\""));
        // (The Replication legend entry now carries a per-socket-color note, so
        // match its prefix rather than an exact `>Replication<`.)
        assert!(html.contains(">SIP<") && html.contains(">Replication"));
        assert!(html.contains("Lifecycle"));
    }

    #[test]
    fn format_epoch_utc_renders_time_of_day_with_millis() {
        // 1970-01-01 + (12h34m56.789s) → 12:34:56.789Z.
        let ms = ((12 * 3600 + 34 * 60 + 56) * 1000 + 789) as i64;
        assert_eq!(format_epoch_utc(ms), "12:34:56.789Z");
        // Wraps within the day (only time-of-day is shown).
        assert_eq!(format_epoch_utc(ms + 86_400_000), "12:34:56.789Z");
    }

    #[test]
    fn wall_clock_doc_shows_absolute_utc_reference() {
        // A wall-clock-aligned doc renders the absolute-UTC anchor in the header
        // AND an absolute time next to each row's relative stamp. A virtual-clock
        // doc (epoch_base_ms = None) shows neither (relative-only, unchanged).
        let mut doc = mixed_doc();
        // Anchor the base (at_ms 0) to a known epoch ms.
        let t0 = ((8 * 3600 + 48 * 60 + 29) * 1000 + 135) as i64; // 08:48:29.135Z
        doc.epoch_base_ms = Some(t0);
        let html = render_html(&doc);
        assert!(html.contains("Timeline t0"), "header states the absolute anchor");
        assert!(html.contains("08:48:29.135Z"), "absolute UTC anchor shown");
        // The repl row at at_ms=200 → t0 + 200ms = 08:48:29.335Z somewhere.
        assert!(html.contains("08:48:29.335Z"), "per-row absolute time shown");

        // Without the anchor, no absolute time leaks in (relative-only, unchanged).
        let plain = render_html(&mixed_doc());
        assert!(!plain.contains("Timeline t0"));
        assert!(!plain.contains("08:48:29"));
    }

    #[test]
    fn lanes_are_columns_in_declared_order() {
        let doc = mixed_doc();
        let html = render_html(&doc);
        let a = html.find("alice (lane)").unwrap();
        let b1 = html.find("b1 (lane)").unwrap();
        let b2 = html.find("b2 (lane)").unwrap();
        assert!(a < b1 && b1 < b2, "columns in declared order");
    }

    #[test]
    fn two_pane_layout_with_fixed_detail_panel() {
        // The viewport-filling two-pane shell: a scrollable diagram on the left,
        // a FIXED, always-visible Message-Detail panel on the right whose
        // scrollable `.detail-body` shows the clicked payload.
        let doc = mixed_doc();
        let html = render_html(&doc);
        assert!(html.contains("class=\"main\""), "two-pane flex container present");
        assert!(html.contains("class=\"diagram-panel\""), "left diagram panel present");
        assert!(html.contains("class=\"detail-panel\""), "right detail panel present");
        assert!(html.contains("class=\"detail-body\""), "scrollable detail body present");
        assert!(html.contains("Message Detail"), "detail header captioned");
        assert!(
            html.contains("class=\"detail-placeholder\""),
            "placeholder shown until a message is clicked"
        );
    }

    #[test]
    fn payload_lives_in_hidden_block_keyed_by_message_ordinal() {
        // The full wire text lives in a HIDDEN, HTML-escaped block keyed by the
        // message's diagram ordinal: `<div class="payload" id="evt-{N}" hidden>`.
        let doc = mixed_doc();
        let html = render_html(&doc);
        assert!(html.contains("class=\"payload "), "payload block present");
        assert!(html.contains("id=\"evt-0\""), "payload keyed by ordinal");
        // It is hidden (the detail panel pulls its innerHTML on click).
        let pay = html.find("id=\"evt-0\"").expect("evt-0 present");
        let pay_block = &html[pay..pay + html[pay..].find("</div>").unwrap_or(html.len() - pay)];
        // The opening tag of the evt-0 block carries `hidden`.
        let open_end = html[pay..].find('>').unwrap() + pay;
        assert!(
            html[pay..open_end].contains("hidden"),
            "payload block is hidden: {}",
            &html[pay..open_end]
        );
        let _ = pay_block;
        assert!(html.contains("INVITE sip:bob SIP/2.0"), "wire detail embedded");
    }

    #[test]
    fn diagram_message_is_a_seq_msg_keyed_to_its_payload_block() {
        // A SIP message in the SVG diagram is a clickable `<g class="seq-msg"
        // data-idx="N">` whose ordinal matches its `#evt-N` payload block. The
        // click `<script>` wires `.seq-msg` → `.detail-body`.
        let doc = mixed_doc();
        let html = render_html(&doc);

        // INVITE sorts first (at_ms 0) → ord 0.
        let svg_end = html.find("</svg>").expect("svg present");
        let svg = &html[..svg_end];
        assert!(
            svg.contains("class=\"seq-msg seq-sip\" data-idx=\"0\""),
            "first message is a .seq-msg group with data-idx=\"0\": {svg}"
        );
        // The group with data-idx="0" wraps the INVITE arrow label.
        let g = svg
            .find("data-idx=\"0\"")
            .expect("first message has data-idx=0");
        let g_close = svg[g..].find("</g>").expect("group closes");
        let group_span = &svg[g..g + g_close];
        assert!(
            group_span.contains("INVITE sip:bob"),
            "the data-idx=0 group carries the INVITE arrow label: {group_span}"
        );
        // It contains a transparent full-row hit rect so the whole row clicks.
        assert!(
            group_span.contains("fill=\"transparent\""),
            "group has a transparent hit target: {group_span}"
        );

        // The matching hidden payload block (id=evt-0) carries the full wire text.
        let pay = html.find("id=\"evt-0\"").expect("payload evt-0 present");
        assert!(
            html[pay..].contains("INVITE sip:bob SIP/2.0"),
            "payload evt-0 holds the message's full detail"
        );

        // The click handler wires `.seq-msg` clicks into the `.detail-body`.
        assert!(
            html.contains("querySelectorAll('.seq-msg')"),
            "click handler binds .seq-msg groups"
        );
        assert!(
            html.contains("getElementById('evt-' + g.dataset.idx)"),
            "click handler resolves the matching #evt-{{idx}} payload"
        );
        assert!(
            html.contains("querySelector('.detail-body').innerHTML"),
            "click handler populates the .detail-body"
        );
    }

    #[test]
    fn embed_is_a_self_contained_clickable_fragment() {
        // `render_embed` is the host-embeddable view (the E2E cell page): no
        // `<html>` chrome, but the SVG messages ARE wired to reveal their
        // payload on click — everything scoped under `.seq-embed`.
        let doc = mixed_doc();
        let embed = render_embed(&doc);

        // A fragment, not a full document.
        assert!(!embed.contains("<!DOCTYPE"), "embed is a fragment, no doctype");
        assert!(embed.contains("class=\"seq-embed\""), "scoped root present");
        // The clickable message group + its matching hidden payload block.
        assert!(
            embed.contains("class=\"seq-msg seq-sip\" data-idx=\"0\""),
            "first message is a clickable group: {embed}"
        );
        assert!(embed.contains("id=\"evt-0\""), "matching payload block keyed by ordinal");
        assert!(embed.contains("INVITE sip:bob SIP/2.0"), "payload carries the wire text");
        // The detail pane + a click handler scoped to the embed root (so a host
        // page with other content is never touched).
        assert!(embed.contains("class=\"detail-body\""), "detail pane present");
        assert!(
            embed.contains("document.currentScript.closest('.seq-embed')"),
            "click handler scopes to the embed root"
        );
    }

    #[test]
    fn status_and_anomalies_surface() {
        let mut doc = mixed_doc();
        doc.passed = false;
        doc.anomalies.push(Anomaly {
            check: "rfc.cseqInDialogOrder".into(),
            detail: "out of order".into(),
            lane: Some("b1".into()),
            endpoint: None,
            advisory: Some(false),
        });
        let html = render_html(&doc);
        assert!(html.contains("FAIL"));
        assert!(html.contains("rfc.cseqInDialogOrder"));
        let txt = render_global_txt(&doc);
        assert!(txt.contains("FAIL"));
        assert!(txt.contains("rfc.cseqInDialogOrder"));
    }

    /// 036 ask C: consecutive lanes sharing a `group` render one bracketing
    /// socket header above their individual captions.
    #[test]
    fn grouped_sub_lanes_render_a_shared_socket_header() {
        let mut doc = mixed_doc();
        doc.lanes = vec![
            lane("alice", LaneKind::Ua),
            Lane::new("10.0.0.9:5070#callee", "callee", LaneKind::Ua).with_group("10.0.0.9:5070"),
            Lane::new("10.0.0.9:5070#alt", "alt", LaneKind::Ua).with_group("10.0.0.9:5070"),
        ];
        doc.rows.truncate(0);
        doc.rows.push(SeqRow {
            at_ms: 0,
            seq: 1,
            from: "alice".into(),
            to: Some("10.0.0.9:5070#callee".into()),
            label: "INVITE sip:x".into(),
            detail: None,
            conn: None,
            kind: RowKind::Sip { delivered: true },
        });
        let html = render_html(&doc);
        // The shared socket appears once as the group header; both sub-lane
        // captions render individually.
        assert!(html.contains("10.0.0.9:5070</text>"), "group header text:\n{html}");
        assert!(html.contains(">callee</text>"));
        assert!(html.contains(">alt</text>"));
    }

    /// 036 ask C: SIP rows carrying a `conn` (the Call-ID) are coloured per
    /// flow and named in the legend; the long id never renders inline on the
    /// arrow label.
    #[test]
    fn sip_rows_colour_by_call_id_with_legend() {
        let mut doc = mixed_doc();
        let cid = "b-1-1234567890abcdef@10.244.0.7";
        doc.rows[1].conn = Some(cid.into());
        let html = render_html(&doc);
        // Legend chip names the (truncated) Call-ID.
        assert!(html.contains("Call-ID b-1-1234567890abcdef@10.244…"), "legend chip:\n{html}");
        // The arrow label itself stays clean (no inline Call-ID).
        assert!(html.contains(">INVITE sip:bob</text>"), "inline label unchanged:\n{html}");
        // The arrow is drawn with a conn-coloured arrowhead, not the SIP one.
        assert!(html.contains("url(#ah-conn-"), "conn-coloured arrowhead used:\n{html}");
    }
}
