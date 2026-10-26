//! What a puller is handed beside its stream: the router's command sink, the
//! memory admission gate and the refusal it tells the primary there, and the
//! turn-point seam it reports applied writes to.

use repl_net::frame::{Frame, Op, Watermark};
use repl_net::transport::ReplicationConnection;

use super::apply::{ApplyMode, ApplyOutcome};
use super::{Puller, RouterSink};

impl Puller {
    /// Attach the router command sink (ADR-0011 X11 fail-back). A puller built
    /// without it (sim + unit tests, which assert on the store directly) sends no
    /// router commands.
    pub fn with_repl_sink(mut self, sink: impl Into<RouterSink>) -> Self {
        self.repl_tx = Some(sink.into());
        self
    }

    /// Attach the memory admission gate whose backup ceilings this puller
    /// applies (ADR-0037).
    pub fn with_capacity(mut self, gate: crate::capacity::CapacityGate) -> Self {
        self.capacity = Some(gate);
        self
    }

    /// State this worker's `identity` on every flow it opens (ADR-0055 §6).
    pub fn with_identity(mut self, identity: Option<String>) -> Self {
        self.identity = identity;
        self
    }

    /// Report every write of a primary this puller applies to `probe`
    /// (ADR-0055 §13).
    pub fn with_turn_probe(mut self, probe: crate::turn_probe::TurnProbes) -> Self {
        self.turn_probe = probe;
        self
    }

    /// Report a primary's write this backup applied to the turn-point seam
    /// (ADR-0055 §13).
    pub(super) async fn report_applied(
        &self,
        mode: ApplyMode,
        outcome: ApplyOutcome,
        op: Op,
        call_ref: &str,
        version: (i64, i64),
    ) {
        if mode != ApplyMode::Forward || outcome != ApplyOutcome::Applied {
            return;
        }
        self.turn_probe
            .at(|| crate::turn_probe::TurnMark {
                node: &self.self_ordinal,
                call_ref,
                point: crate::turn_probe::TurnPoint::Applied,
                version,
                wrote: true,
                removed: matches!(op, Op::Delete),
                on_request: false,
                called_out: false,
            })
            .await;
    }

    /// Refuse the replica of call `incarnation` on `call_ref` at this backup's
    /// ceiling and tell the primary, whose write at `at` it was (ADR-0037 item
    /// 4). `false` when the frame could not be sent.
    pub(super) async fn refuse(
        &self,
        conn: &dyn ReplicationConnection,
        at: Watermark,
        call_ref: &str,
        incarnation: Option<&str>,
    ) -> bool {
        self.store.refuse_replica(call_ref, self.peer_ordinal(), incarnation);
        let frame = Frame::ReplicaRefused {
            at,
            call_ref: call_ref.to_string(),
            incarnation: incarnation.map(str::to_string),
        };
        self.send(conn, frame).await
    }
}
