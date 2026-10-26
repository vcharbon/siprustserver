//! The per-call recorder ([`CallCtx`]): checkpoints, phases, the ringing
//! outcome, message anchors, sample notes, and the moment the call's verdict
//! resolved.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::anchors::{AnchorKeys, AnchorTag};
use crate::Agent;

/// Per-call timing recorder. Holds the call's start instant, the named
/// **checkpoints** ("keywords") a scenario marks at points whose latency we want
/// to track (e.g. `time_to_200`), and the **phase markers** a scenario stamps as
/// it advances (connected, re-invited, transferred, post-PRACK, …) so a NOK
/// sample can say WHICH phase was live. `Mutex` (not `RefCell`) so it is `Sync` — the `async-trait`
/// scenario future borrows `&CallCtx` across awaits.
pub struct CallCtx {
    t0: Instant,
    checkpoints: Mutex<Vec<(&'static str, Duration)>>,
    phases: Mutex<Vec<(&'static str, Instant)>>,
    /// Whether the caller received the `18x` ringing provisional before the answer.
    /// `None` until the call reaches the ring→answer step; `Some(true)` if the 18x
    /// arrived, `Some(false)` if it was legitimately lost (a NON-PRACK provisional
    /// is best-effort, not a call failure — the driver aggregates the rate into a
    /// cross-call gate instead of failing the individual call).
    ringing: Mutex<Option<bool>>,
    /// Message-anchor collection — the load surface's analogue of
    /// [`Harness::tag_anchor`](crate::Harness::tag_anchor) (ADR-0019). OFF by
    /// default: [`anchor`](Self::anchor) is then a single relaxed atomic load
    /// (~free on the unsampled load majority and in the functional leak gate).
    /// The load driver enables it on a SAMPLED (recording) call so the e2e
    /// check engine can resolve `<agent>.<anchor>` over the recorded trace.
    collect_anchors: AtomicBool,
    anchors: Mutex<Vec<AnchorTag>>,
    /// Free-form per-call diagnostic notes for the SAMPLE DETAIL channel only —
    /// e.g. the settle barrier's still-open obligation names. NEVER keyed (the
    /// case discriminator stays the failed step); the driver appends them to a NOK
    /// sample's failure detail so the rendered page explains WHY.
    notes: Mutex<Vec<String>>,
    /// Run once when the call's verdict resolves, before its teardown
    /// ([`on_resolved`](Self::on_resolved)).
    on_resolved: Mutex<Option<ResolveHook>>,
    /// Whether a party the system under test ends cleanly ends the call
    /// cleanly ([`accept_clean_ends`](Self::accept_clean_ends)).
    clean_ends: AtomicBool,
}

/// What a call runs at the instant its verdict resolves.
type ResolveHook = Box<dyn FnOnce() + Send>;

impl CallCtx {
    pub fn new() -> Self {
        Self {
            t0: Instant::now(),
            checkpoints: Mutex::new(Vec::new()),
            phases: Mutex::new(Vec::new()),
            ringing: Mutex::new(None),
            collect_anchors: AtomicBool::new(false),
            anchors: Mutex::new(Vec::new()),
            notes: Mutex::new(Vec::new()),
            on_resolved: Mutex::new(None),
            clean_ends: AtomicBool::new(false),
        }
    }

    /// From now on, a party the system under test ends cleanly — a non-2xx
    /// final to her establishing INVITE, a BYE she answers 200 — drops her
    /// remaining goals, and a caller ended cleanly ends the call cleanly: a
    /// fault injection that may make the system under test end a call arms
    /// it when the fault strikes. A request left without its final or a
    /// dialog left open still fails the call.
    pub fn accept_clean_ends(&self) {
        self.clean_ends.store(true, Ordering::Relaxed);
    }

    /// Whether [`accept_clean_ends`](Self::accept_clean_ends) was armed.
    pub fn clean_ends_accepted(&self) -> bool {
        self.clean_ends.load(Ordering::Relaxed)
    }

    /// Run `hook` at the instant the call's verdict resolves: after its last
    /// scripted step and before the runner's teardown (the best-effort CANCEL
    /// or BYE of a failed call). Replaces an earlier hook.
    pub fn on_resolved(&self, hook: impl FnOnce() + Send + 'static) {
        *self.on_resolved.lock().unwrap() = Some(Box::new(hook));
    }

    /// The call's verdict resolved: run the [`on_resolved`](Self::on_resolved)
    /// hook, once.
    pub fn resolved(&self) {
        let hook = self.on_resolved.lock().unwrap().take();
        if let Some(hook) = hook {
            hook();
        }
    }

    /// Turn message-anchor collection ON for this call (see the field docs).
    /// The load driver calls this exactly when the call is sampled (recording),
    /// so anchors always have a recorded trace to resolve against.
    pub fn enable_anchor_collection(&self) {
        self.collect_anchors.store(true, Ordering::Relaxed);
    }

    /// Label a message `agent` just RECEIVED with a canonical anchor name
    /// (ADR-0019) — the load-surface analogue of `Harness::tag_anchor` /
    /// e2e-core's `InfraRuntime::anchor`. A no-op (one relaxed atomic load,
    /// `keys` never converted) unless anchor collection was enabled. `name` is
    /// `'static` to keep the vocabulary fixed, like phases/checkpoints.
    pub fn anchor(&self, agent: &Agent, name: &'static str, keys: impl Into<AnchorKeys>) {
        self.push_anchor(agent, name, keys, false);
    }

    /// Label a message `agent` just SENT — for a request whose only receiver is
    /// the external SUT (the REFER anchor). See [`AnchorTag::sent`].
    pub fn anchor_sent(&self, agent: &Agent, name: &'static str, keys: impl Into<AnchorKeys>) {
        self.push_anchor(agent, name, keys, true);
    }

    fn push_anchor(
        &self,
        agent: &Agent,
        name: &'static str,
        keys: impl Into<AnchorKeys>,
        sent: bool,
    ) {
        if !self.collect_anchors.load(Ordering::Relaxed) {
            return;
        }
        self.anchors.lock().unwrap().push(AnchorTag {
            agent: agent.name().to_string(),
            anchor: name.to_string(),
            agent_addr: agent.addr(),
            keys: keys.into(),
            sent,
        });
    }

    /// Drain the collected `(agent, anchor)` labels, in tag order (empty unless
    /// [`enable_anchor_collection`](Self::enable_anchor_collection) was called).
    pub fn take_anchors(&self) -> Vec<AnchorTag> {
        std::mem::take(&mut *self.anchors.lock().unwrap())
    }

    pub fn checkpoint(&self, name: &'static str) {
        self.checkpoints.lock().unwrap().push((name, self.t0.elapsed()));
    }

    /// The instant the call started (its first INVITE).
    pub fn start_instant(&self) -> Instant {
        self.t0
    }

    /// Mark that the call reached a named lifecycle phase (e.g. `"connected"`,
    /// `"reinvited"`, `"transferred"`, `"pracked"`), stamping the instant. Cheap
    /// and unconditional; the driver folds these into the NOK sample banner.
    /// `name` is `'static` to keep
    /// cardinality bounded (a fixed phase vocabulary, like checkpoints).
    pub fn phase(&self, name: &'static str) {
        self.phases.lock().unwrap().push((name, Instant::now()));
    }

    /// The recorded phase markers `(name, instant)`, in order reached.
    pub fn phases(&self) -> Vec<(&'static str, Instant)> {
        self.phases.lock().unwrap().clone()
    }

    /// Record whether this call's caller saw its `18x` ringing provisional
    /// (`true`) or it was lost (`false`) — a non-PRACK provisional is best-effort,
    /// so a miss is EXPECTED, not a failure. The driver folds this into the
    /// cross-call `loadgen_ringing_{expected,received}_total` gate (>99%).
    pub fn mark_ringing(&self, received: bool) {
        *self.ringing.lock().unwrap() = Some(received);
    }

    /// The recorded ringing outcome: `None` if the call never reached the answer
    /// step, else `Some(received)`.
    pub fn ringing(&self) -> Option<bool> {
        *self.ringing.lock().unwrap()
    }

    pub fn elapsed(&self) -> Duration {
        self.t0.elapsed()
    }

    pub fn take_checkpoints(&self) -> Vec<(&'static str, Duration)> {
        std::mem::take(&mut self.checkpoints.lock().unwrap())
    }

    /// Attach a free-form diagnostic note for the sample DETAIL channel (see
    /// the field docs — never a report key; the settle verdict's open-obligation
    /// names ride this).
    pub fn note(&self, text: impl Into<String>) {
        self.notes.lock().unwrap().push(text.into());
    }

    /// The attached diagnostic notes, in attach order.
    pub fn notes(&self) -> Vec<String> {
        self.notes.lock().unwrap().clone()
    }
}

impl Default for CallCtx {
    fn default() -> Self {
        Self::new()
    }
}
