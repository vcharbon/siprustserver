//! The decision plane of a run: each request a worker sends its routing
//! decision engine (a new call, a failed leg, a transfer, a release consult)
//! and the answer it gets back, recorded with the node, the instant and a
//! place in the run's recording order. The unified report draws them on a
//! lane of their own, so a call that waits on its routing decision shows the
//! wait. A request whose node died before the answer came shows unanswered.

use std::fmt::Debug;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use b2bua::decision::{
    CallDecisionEngine, CallDecisionError, CallFailureRequest, CallFailureResponse,
    CallReferRequest, CallReferResponse, CallReleaseRequest, CallReleaseResponse, NewCallRequest,
    NewCallResponse,
};
use seq_report::{Lane, LaneKind, RowKind, SeqDoc, SeqRow};
use sip_clock::Clock;

use crate::turn_points::Stamp;

/// The decision engine's lane id in the unified report.
pub const LANE: &str = "routing";

/// One request to the decision engine, or its answer.
#[derive(Clone, Debug)]
struct Event {
    at_ms: i64,
    seq: u64,
    node: String,
    /// The request this event belongs to.
    id: u64,
    answer: bool,
    label: String,
    detail: String,
}

/// Every decision request of a run and its answer.
#[derive(Clone)]
pub struct Callouts {
    events: Arc<Mutex<Vec<Event>>>,
    next: Arc<AtomicU64>,
    stamp: Stamp,
    clock: Clock,
}

impl Callouts {
    /// A log placing its events with `stamp`, timed by `clock`.
    pub fn new(stamp: Stamp, clock: Clock) -> Self {
        let (events, next) = (Arc::default(), Arc::default());
        Self { events, next, stamp, clock }
    }

    /// `engine` as the worker `node` sees it, each request and answer logged.
    pub fn wrap(
        &self,
        engine: Arc<dyn CallDecisionEngine>,
        node: &str,
    ) -> Arc<dyn CallDecisionEngine> {
        Arc::new(Recorded { inner: engine, node: node.to_string(), log: self.clone() })
    }

    fn push(&self, node: &str, id: u64, answer: bool, label: String, detail: String) {
        let event = Event {
            at_ms: self.clock.now_ms(),
            seq: (self.stamp)(),
            node: node.to_string(),
            id,
            answer,
            label,
            detail,
        };
        self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(event);
    }

    /// Log `node`'s request `what` (`req`), await `answer`, and log it.
    async fn around<A: Debug>(
        &self,
        node: &str,
        what: &str,
        req: String,
        answer: impl Future<Output = Result<A, CallDecisionError>>,
    ) -> Result<A, CallDecisionError> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.push(node, id, false, format!("decision: {what}"), req);
        let out = answer.await;
        let (label, detail) = match &out {
            Ok(a) => (variant(&format!("{a:?}")), format!("{a:#?}")),
            Err(e) => ("error".to_string(), format!("{e:#?}")),
        };
        self.push(node, id, true, format!("{what} → {label}"), detail);
        out
    }

    /// Draw every logged event on `doc`: a request from its node's column to
    /// the decision engine's, unanswered when its node died first, and the
    /// answer back. Adds the lane when the run asked anything.
    pub fn draw(&self, doc: &mut SeqDoc) {
        let events = self.events.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        if events.is_empty() {
            return;
        }
        doc.lanes.push(Lane::new(LANE, "routing (decision engine)", LaneKind::Service));
        let answered: Vec<u64> = events.iter().filter(|e| e.answer).map(|e| e.id).collect();
        for e in events {
            let node = node_lane(doc, &e.node);
            let (from, to) = if e.answer { (LANE.to_string(), node) } else { (node, LANE.into()) };
            let delivered = e.answer || answered.contains(&e.id);
            doc.rows.push(SeqRow {
                at_ms: e.at_ms,
                seq: e.seq,
                from,
                to: Some(to),
                label: e.label,
                detail: Some(e.detail),
                conn: None,
                kind: RowKind::Http { delivered },
            });
        }
    }
}

/// The column of the worker `ordinal`: its own, else the first of its
/// incarnations.
fn node_lane(doc: &SeqDoc, ordinal: &str) -> String {
    doc.lanes
        .iter()
        .find(|l| l.id == ordinal)
        .or_else(|| doc.lanes.iter().find(|l| l.group.as_deref() == Some(ordinal)))
        .map_or_else(|| ordinal.to_string(), |l| l.id.clone())
}

/// The variant name a `Debug` rendering starts with.
fn variant(debug: &str) -> String {
    debug.split(['(', '{', ' ']).next().unwrap_or(debug).to_string()
}

/// A decision engine whose every request and answer is logged.
struct Recorded {
    inner: Arc<dyn CallDecisionEngine>,
    node: String,
    log: Callouts,
}

#[async_trait]
impl CallDecisionEngine for Recorded {
    async fn new_call(&self, req: NewCallRequest) -> Result<NewCallResponse, CallDecisionError> {
        let detail = format!("{req:#?}");
        self.log.around(&self.node, "new call", detail, self.inner.new_call(req)).await
    }

    async fn call_failure(
        &self,
        req: CallFailureRequest,
    ) -> Result<CallFailureResponse, CallDecisionError> {
        let detail = format!("{req:#?}");
        self.log.around(&self.node, "failure", detail, self.inner.call_failure(req)).await
    }

    async fn call_refer(
        &self,
        req: CallReferRequest,
    ) -> Result<CallReferResponse, CallDecisionError> {
        let detail = format!("{req:#?}");
        self.log.around(&self.node, "refer", detail, self.inner.call_refer(req)).await
    }

    async fn call_release(
        &self,
        req: CallReleaseRequest,
    ) -> Result<CallReleaseResponse, CallDecisionError> {
        let detail = format!("{req:#?}");
        self.log.around(&self.node, "release", detail, self.inner.call_release(req)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_debug_rendering_names_its_variant() {
        assert_eq!(variant("Route(RouteDecision { a: 1 })"), "Route");
        assert_eq!(variant("Reject { status: 486 }"), "Reject");
        assert_eq!(variant("Release"), "Release");
    }
}
