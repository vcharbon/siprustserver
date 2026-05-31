//! `Recorder` — single source of truth for what crossed the test fabric
//! (port of `report-recorder/Recorder.ts`).
//!
//! Two surfaces:
//!
//!   - **Typed channels** ([`Recorder::for_tag`] → [`Channel`]). Each layer's
//!     decorator opens one channel keyed by a stable `&'static str` and records
//!     its event union onto it. The channel stamps `seq` (shared sequencer) +
//!     `at_ms` on every record. Re-opening the same key returns the same
//!     buffer.
//!   - **Anomaly ledger** ([`Recorder::record_anomaly`] for eager findings,
//!     [`Recorder::register_projector`] for findings derived at snapshot from a
//!     channel). [`Recorder::snapshot`] drains lanes + the merged anomalies.
//!
//! Recording is synchronous (the TS `record` was `Effect.sync` over an
//! in-memory array), so none of this is `async` — a `Drop` finalizer can
//! record cleanly.

use std::any::Any;
use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use sip_clock::Clock;

use crate::anomaly::{RecordedAnomaly, Severity};
use crate::event_sequencer::EventSequencer;
use crate::scenario::{
    lane_key, Lane, LaneKey, LaneRegistry, MutableLane, NetworkTag, RecordedScenario, TransportKind,
};

/// A recorded event plus the bookkeeping the channel stamps on capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamped<E> {
    pub event: E,
    pub seq: u64,
    pub at_ms: u64,
}

/// A typed event channel for one layer (one `tag` key). Cloning the handle
/// shares the underlying buffer + sequencer, so a decorator can hand clones to
/// each wrapped endpoint and they all append to one ordered log.
pub struct Channel<E> {
    buf: Arc<Mutex<Vec<Stamped<E>>>>,
    seq: Arc<EventSequencer>,
    clock: Clock,
}

impl<E> Clone for Channel<E> {
    fn clone(&self) -> Self {
        Self {
            buf: self.buf.clone(),
            seq: self.seq.clone(),
            clock: self.clock.clone(),
        }
    }
}

impl<E> Channel<E> {
    /// Append one event. `seq` + `at_ms` are stamped here so callers pass only
    /// the layer-specific payload. `at_ms` rides the injected [`Clock`]
    /// (monotonic-anchored), so under a paused tokio runtime it advances with
    /// `tokio::time::advance` — deterministic report timestamps. `seq` remains
    /// the ordering authority; `at_ms` is for the renderer's `(at_ms, seq)`
    /// sort and the relative-time labels.
    pub fn record(&self, event: E) {
        let seq = self.seq.next();
        let at_ms = self.clock.now_ms().max(0) as u64;
        self.buf
            .lock()
            .unwrap()
            .push(Stamped { event, seq, at_ms });
    }
}

impl<E: Clone> Channel<E> {
    /// A clone of every event recorded so far, in capture order.
    pub fn snapshot(&self) -> Vec<Stamped<E>> {
        self.buf.lock().unwrap().clone()
    }
}

type Projector = Box<dyn Fn() -> Vec<RecordedAnomaly> + Send + Sync>;

struct RecorderState {
    lanes: LaneRegistry,
    pending_kills: HashMap<LaneKey, Vec<u64>>,
    /// Eagerly-recorded findings (layer-close pushes, lane conflicts).
    anomalies: Vec<RecordedAnomaly>,
    /// Type-erased channel buffers keyed by tag. The concrete type is
    /// `Arc<Mutex<Vec<Stamped<E>>>>`, recovered by `for_tag::<E>` via downcast.
    channels: HashMap<&'static str, Arc<dyn Any + Send + Sync>>,
    /// Projectors run at snapshot to derive findings from a channel. First
    /// registration per tag wins.
    projectors: HashMap<&'static str, Projector>,
    conflicts_reported: HashSet<LaneKey>,
}

/// The recorder handle. Clone is cheap (shared `Arc`); every clone sees the
/// same lanes, channels, and ledger.
#[derive(Clone)]
pub struct Recorder {
    state: Arc<Mutex<RecorderState>>,
    seq: Arc<EventSequencer>,
    kind: TransportKind,
    clock: Clock,
}

impl Recorder {
    /// Build a recorder with the given transport kind. Prefer the named
    /// constructors below so the kind is self-documenting at the call site.
    /// Uses [`Clock::system`] for timestamps — production/real-time runs.
    pub fn new(kind: TransportKind) -> Self {
        Self::with_sequencer(kind, Arc::new(EventSequencer::new()))
    }

    /// Build a recorder sharing an external sequencer — use when several
    /// recording surfaces must interleave into one global `seq` order.
    pub fn with_sequencer(kind: TransportKind, seq: Arc<EventSequencer>) -> Self {
        Self::with_clock_and_sequencer(kind, Clock::system(), seq)
    }

    /// Build a recorder whose timestamps ride `clock`. Pass a
    /// [`Clock::test_at`] under a paused tokio runtime for fully deterministic
    /// report timestamps (the scenario harness does this).
    pub fn with_clock(kind: TransportKind, clock: Clock) -> Self {
        Self::with_clock_and_sequencer(kind, clock, Arc::new(EventSequencer::new()))
    }

    /// Most general constructor: explicit clock + sequencer.
    pub fn with_clock_and_sequencer(
        kind: TransportKind,
        clock: Clock,
        seq: Arc<EventSequencer>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(RecorderState {
                lanes: LaneRegistry::new(),
                pending_kills: HashMap::new(),
                anomalies: Vec::new(),
                channels: HashMap::new(),
                projectors: HashMap::new(),
                conflicts_reported: HashSet::new(),
            })),
            seq,
            kind,
            clock,
        }
    }

    pub fn fake() -> Self {
        Self::new(TransportKind::Fake)
    }
    pub fn live() -> Self {
        Self::new(TransportKind::Live)
    }
    pub fn hybrid() -> Self {
        Self::new(TransportKind::Hybrid)
    }

    /// The shared sequencer, so a layer can stamp `seq` on findings it builds
    /// outside a channel (e.g. layer-close anomalies) from the same counter.
    pub fn sequencer(&self) -> Arc<EventSequencer> {
        self.seq.clone()
    }

    /// The timestamp clock — for decorators that stamp findings/packets outside
    /// a channel and want them on the same timeline as the recorded events.
    pub fn clock(&self) -> Clock {
        self.clock.clone()
    }

    // ---- typed channels -------------------------------------------------

    /// Open (or re-open) the typed channel for `tag`. Re-opening with the same
    /// key returns a handle to the same buffer; opening with a different `E`
    /// for an existing key is a programmer error and panics on the downcast.
    pub fn for_tag<E: Send + Sync + 'static>(&self, tag: &'static str) -> Channel<E> {
        let mut st = self.state.lock().unwrap();
        if let Some(any) = st.channels.get(tag) {
            let buf = any
                .clone()
                .downcast::<Mutex<Vec<Stamped<E>>>>()
                .expect("Recorder::for_tag called with a different event type for the same tag");
            return Channel {
                buf,
                seq: self.seq.clone(),
                clock: self.clock.clone(),
            };
        }
        let buf: Arc<Mutex<Vec<Stamped<E>>>> = Arc::new(Mutex::new(Vec::new()));
        st.channels
            .insert(tag, buf.clone() as Arc<dyn Any + Send + Sync>);
        Channel {
            buf,
            seq: self.seq.clone(),
            clock: self.clock.clone(),
        }
    }

    // ---- anomaly ledger -------------------------------------------------

    /// Append a finding eagerly (used by layer-close finalizers that compute a
    /// finding outside a channel projection).
    pub fn record_anomaly(&self, anomaly: RecordedAnomaly) {
        self.state.lock().unwrap().anomalies.push(anomaly);
    }

    /// Register a projector for `tag`. It runs at [`Recorder::snapshot`] and
    /// its findings merge into the scenario's anomaly list. First registration
    /// per tag wins (later ones are ignored).
    pub fn register_projector<F>(&self, tag: &'static str, projector: F)
    where
        F: Fn() -> Vec<RecordedAnomaly> + Send + Sync + 'static,
    {
        let mut st = self.state.lock().unwrap();
        if st.projectors.contains_key(tag) {
            return;
        }
        st.projectors.insert(tag, Box::new(projector));
    }

    // ---- lane registry --------------------------------------------------

    /// Register an `addr → name` lane mapping. Idempotent on the name.
    /// Registering a *different* name on an existing lane records both names
    /// and queues a `nameConflict` anomaly (once per lane).
    pub fn register_lane(&self, addr: SocketAddr, name: impl Into<String>, network: NetworkTag) {
        let name = name.into();
        let key = lane_key(addr);
        let mut st = self.state.lock().unwrap();
        match st.lanes.get_mut(&key) {
            None => {
                let initial_kills = st.pending_kills.remove(&key).unwrap_or_default();
                st.lanes.insert(
                    key,
                    MutableLane {
                        addr,
                        names: vec![name],
                        network,
                        killed_at: initial_kills,
                    },
                );
            }
            Some(lane) => {
                if lane.names.contains(&name) {
                    return;
                }
                lane.names.push(name);
                let names = lane.names.clone();
                if st.conflicts_reported.insert(key.clone()) {
                    let seq = self.seq.next();
                    let at_ms = self.clock.now_ms().max(0) as u64;
                    st.anomalies.push(RecordedAnomaly::new(
                        "nameConflict",
                        "lane.nameConflict",
                        format!("lane {key} labelled with conflicting names: {names:?}"),
                        Severity::Advisory,
                        Some(key),
                        seq,
                        at_ms,
                    ));
                }
            }
        }
    }

    /// `register_lane` with the network defaulted to `ext`.
    pub fn label_lane(&self, addr: SocketAddr, name: impl Into<String>) {
        self.register_lane(addr, name, NetworkTag::Ext);
    }

    /// Mark a lane killed at `at`. On an unregistered lane the timestamp is
    /// buffered and merged when the lane is later registered.
    pub fn mark_lane_killed(&self, addr: SocketAddr, at: u64) {
        let key = lane_key(addr);
        let mut st = self.state.lock().unwrap();
        if let Some(lane) = st.lanes.get_mut(&key) {
            lane.killed_at.push(at);
        } else {
            st.pending_kills.entry(key).or_default().push(at);
        }
    }

    // ---- drain ----------------------------------------------------------

    /// Drain into the renderer-facing snapshot: lanes + the merged anomaly
    /// ledger (eager findings ++ every projector's output).
    pub fn snapshot(&self) -> RecordedScenario {
        let st = self.state.lock().unwrap();
        let lanes: Vec<Lane> = st
            .lanes
            .values()
            .map(|l| Lane {
                addr: l.addr,
                names: l.names.clone(),
                network: l.network,
                killed_at: l.killed_at.clone(),
            })
            .collect();
        let mut anomalies = st.anomalies.clone();
        for projector in st.projectors.values() {
            anomalies.extend(projector());
        }
        RecordedScenario {
            transport_kind: self.kind,
            lanes,
            anomalies,
        }
    }

    /// All findings currently on the ledger (eager ++ projected). Convenience
    /// for tests that assert on anomalies without the full scenario.
    pub fn anomalies(&self) -> Vec<RecordedAnomaly> {
        self.snapshot().anomalies
    }
}
