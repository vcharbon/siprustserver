//! The label-keyed families of the worker's counter set, one field each.

use metric_values::{FixedCounts, OpenRows};

/// The label-keyed families of the counter set: a semi-open one's rows (one
/// per observed label set) or a fixed one's counts, every declared label set
/// at 0 from the start.
#[derive(Debug)]
pub(super) struct Rows {
    // Inbound requests by method, outbound requests this worker originated
    // or relayed by method, inbound responses by CSeq method and status.
    pub(super) requests: OpenRows,
    pub(super) requests_out: OpenRows,
    pub(super) responses: OpenRows,
    // Every repeat of a retained emission that left this worker — a rung of
    // a dialog-level ladder or a triggered re-send — by ladder, method and,
    // for a response, code. An original send is not a repeat.
    pub(super) retransmits: OpenRows,
    // Dialog-level ladders that ran to their give-up, by the kind of
    // obligation left undischarged: the peer went deaf.
    pub(super) repeat_give_ups: FixedCounts,
    // Model invariants broken and defects met, by name (ADR-0052 §8).
    pub(super) model_defects: FixedCounts,
    // Turns that crossed a per-call cap, by cap and kind; turns that grew a
    // capped collection across its half, by cap (ADR-0057 §7).
    pub(super) cap_trips: FixedCounts,
    pub(super) cap_near: FixedCounts,
    // Outbound messages that could not leave (`router::cannot_leave`), by
    // reason and kind.
    pub(super) outbound_unsent: FixedCounts,
    // Messages and events that resolved to no call (`router::unroutable`),
    // in three classes that never overlap: wire messages that drew no
    // answer (by kind), wire requests answered on no call's behalf (by
    // method and code), this node's own events naming no call (by event and
    // method).
    pub(super) unroutable_dropped: FixedCounts,
    pub(super) unroutable_refused: OpenRows,
    pub(super) unroutable_internal: OpenRows,
    // Quiet turns persisted without a bump or a flush, by kind.
    pub(super) repl_quiet_turns: FixedCounts,
    // Inbound replication ops applied, by flow, peer and op: a reboot's bulk
    // reclaim is a sharp `recovery`/`create` step for that peer.
    pub(super) repl_applied: OpenRows,
    // Catch-up/idle `Noop`s this node sent as a server, by flow and pulling
    // peer (ADR-0014): climbs on every healthy stream; a flat one names a
    // stuck serve loop.
    pub(super) repl_noops_sent: OpenRows,
    // Forward flushes a backup refused as a regression of its own progress
    // (ADR-0031 D3), by the operation refused.
    pub(super) repl_forward_flush_refused: FixedCounts,
    // Inbound replication connections closed before they opened a flow, by
    // reason (ADR-0045).
    pub(super) repl_inbound_dropped: FixedCounts,
    // Live calls resting at each state-machine cursor (ADR-0016),
    // a census replaced whole on the gauge cadence.
    pub(super) sm_cursors: OpenRows,
}

impl Default for Rows {
    fn default() -> Self {
        use super::catalogue::wire_drops as d;
        use super::catalogue::worker as w;
        Self {
            requests: OpenRows::new(&w::REQUESTS),
            requests_out: OpenRows::new(&w::REQUESTS_OUT),
            responses: OpenRows::new(&w::RESPONSES),
            retransmits: OpenRows::new(&w::RETRANSMITS),
            repeat_give_ups: FixedCounts::new(&w::REPEAT_GIVE_UPS),
            model_defects: FixedCounts::new(&w::MODEL_DEFECTS),
            cap_trips: FixedCounts::new(&super::catalogue::caps::CAP_TRIPS),
            cap_near: FixedCounts::new(&super::catalogue::caps::CAP_NEAR),
            outbound_unsent: FixedCounts::new(&d::OUTBOUND_UNSENT),
            unroutable_dropped: FixedCounts::new(&d::UNROUTABLE_DROPPED),
            unroutable_refused: OpenRows::new(&d::UNROUTABLE_REFUSED),
            unroutable_internal: OpenRows::new(&d::UNROUTABLE_INTERNAL),
            repl_quiet_turns: FixedCounts::new(&w::REPL_QUIET_TURNS),
            repl_applied: OpenRows::new(&w::REPL_APPLIED),
            repl_noops_sent: OpenRows::new(&w::REPL_NOOPS_SENT),
            repl_forward_flush_refused: FixedCounts::new(&w::REPL_FORWARD_FLUSH_REFUSED),
            repl_inbound_dropped: FixedCounts::new(&w::REPL_INBOUND_DROPPED),
            sm_cursors: OpenRows::new(&w::SM_CURSORS),
        }
    }
}
