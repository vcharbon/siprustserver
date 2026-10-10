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
//! Separate per-plane renderers (scenario-harness's SIP HTML/text,
//! ha-harness's replication text/mermaid), concatenated, cannot show the
//! *interleaving* — which is exactly what you need to understand a failover bug: did the crash
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
//! - [`RowKind::Http`] is an HTTP request (requester → a [`LaneKind::Service`]
//!   lane) or its reply (back), drawn in its own style; `delivered` is `false`
//!   for a reply that never came.
//! - [`RowKind::Lifecycle`] is a full-width BAND across all lanes (`to == None`),
//!   a centred annotation in time order (e.g. `crash b1`, `reboot b1`).
//!
//! ## The views plane
//! [`SeqDoc::views`] carries a fourth, optional plane: what each actor BELIEVED
//! about another at each instant ([`ViewChange`]). It renders as a tinted chip on
//! the observer's own column in the sequence, plus a views table and a
//! disagreements list under the diagram — see `views`.

mod html;
mod normalize;
mod sanitize;
mod text;
mod views;

pub use html::{render_embed, render_html, render_svg};
pub use normalize::{normalize, role_map_from_lanes};
pub use sanitize::sanitize_name;
pub use text::render_global_txt;
pub use views::{
    disagreements, views_table, Disagreement, ViewCell, ViewChange, ViewsRow, ViewsTable,
};

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
    /// An HTTP service the actors consult (the requests' destination).
    Service,
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
    /// shared mux socket whose LOGICAL endpoints each get their own sub-lane).
    /// Consecutive lanes with the same `group` render
    /// one bracketing header above their individual captions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

impl Lane {
    /// A lane with no group (see [`Lane::with_group`]).
    pub fn new(id: impl Into<String>, label: impl Into<String>, kind: LaneKind) -> Self {
        Self { id: id.into(), label: label.into(), kind, group: None }
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
    /// An HTTP request or reply.
    Http {
        /// `false` when no reply came back (reset, withheld, abandoned) or the
        /// request reached no service.
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
    /// Global `seq` values ([`SeqRow::seq`]) of the message row(s) this finding
    /// ties to — the offending message(s), when the producing rule can pinpoint
    /// them. The HTML renderer links the finding to those rows (click-to-jump,
    /// per-row badges); empty ⇒ the finding renders unlinked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub row_seqs: Vec<u64>,
    /// Whether the finding came from the RFC-rule audit registry, as opposed to
    /// a structural layer-close finding or a harness note. The projector sets
    /// it — `check` is an opaque token here, so this crate recognizes a rule by
    /// the tag alone. Rule-sourced findings are the only class [`normalize`]
    /// keeps.
    #[serde(default, skip_serializing_if = "is_false")]
    pub rule_sourced: bool,
}

/// `skip_serializing_if` for a `bool` that defaults to false.
fn is_false(b: &bool) -> bool {
    !*b
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
    /// Belief changes on the views plane (empty for a doc that records none):
    /// what each observer held true about each subject, and when it moved. See
    /// the `views` module.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub views: Vec<ViewChange>,
    /// Wall-clock epoch (ms) corresponding to the timeline base (`base_ms`).
    /// `Some` only when `at_ms` is real wall-clock-aligned time (the load driver's
    /// `Clock::system()` recording) — the renderers then show an ABSOLUTE UTC
    /// time alongside the relative `T+…` stamp, so a flow correlates to external
    /// events (e.g. a chaos kill instant). `None` for virtual-clock docs (the
    /// paused-runtime harness/failover views), which stay relative-only.
    ///
    /// `#[serde(default)]` so docs persisted before this field deserialize as
    /// `None` (relative-only).
    #[serde(default)]
    pub epoch_base_ms: Option<i64>,
}

impl SeqDoc {
    /// The rows AND the view-change chips in canonical render order — what both
    /// renderers walk, so HTML and text agree. A view chip occupies an ordinal
    /// like any row (it is not clickable and carries no payload), so the diagram
    /// and the payload blocks stay in lockstep.
    ///
    /// The primary key is `seq`, the GLOBAL recording-order sequence: every
    /// source stamps its rows from one shared counter at the instant each event
    /// is recorded, so `seq` already encodes true append/causal order ACROSS
    /// planes. `at_ms` is only a display label and is NOT the cross-source
    /// tiebreaker — under a paused test clock many events collide on one
    /// millisecond, and ordering those by `at_ms` would mis-order them (e.g. a
    /// reboot marker would float after the bootstrap pull it caused). `at_ms`
    /// breaks a `seq` tie only as a last resort. The sort is stable, so equal-
    /// `seq` entries keep their input order as a final backstop.
    pub(crate) fn sorted_items(&self) -> Vec<Item<'_>> {
        let mut items: Vec<Item<'_>> =
            self.rows.iter().map(Item::Row).chain(self.views.iter().map(Item::View)).collect();
        items.sort_by(|a, b| a.seq().cmp(&b.seq()).then(a.at_ms().cmp(&b.at_ms())));
        items
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

/// One entry on the rendered timeline: a message/lifecycle [`SeqRow`] or a
/// [`ViewChange`] chip. Both renderers walk [`SeqDoc::sorted_items`].
pub(crate) enum Item<'a> {
    Row(&'a SeqRow),
    View(&'a ViewChange),
}

impl Item<'_> {
    pub(crate) fn seq(&self) -> u64 {
        match self {
            Item::Row(r) => r.seq,
            Item::View(v) => v.seq,
        }
    }

    pub(crate) fn at_ms(&self) -> i64 {
        match self {
            Item::Row(r) => r.at_ms,
            Item::View(v) => v.at_ms,
        }
    }

    /// The caption shown on the timeline.
    pub(crate) fn label(&self) -> String {
        match self {
            Item::Row(r) => r.label.clone(),
            Item::View(v) => {
                format!("{}: {} = {} ({})", v.observer, v.subject, v.belief, v.signal)
            }
        }
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
/// — e.g. `T+0.015s`, `T+1m02.345s`. Shared by both renderers so timestamps
/// read identically.
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
mod tests;
