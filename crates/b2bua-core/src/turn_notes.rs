//! [`TurnNotes`]: what a turn reports instead of acting on it (ADR-0052 §4):
//! the trace facts of a sampled call, the counts it owes the metrics, and the
//! defects it met. The shell drains them as the turn's core call returns.

use b2bua_sdk::config::Cap;
use call::Call;

/// Whether this call is traced. The ONE guard every trace fact and emission
/// site uses (ADR-0026).
#[inline]
pub fn sampled(call: &Call) -> bool {
    call.sampled == Some(true)
}

/// One fact of a traced call's story (ADR-0026): its kind, the instant it
/// happened, and what happened in words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceFact {
    pub kind: &'static str,
    pub at_ms: i64,
    pub detail: String,
}

/// An owed PRACK names a leg the call does not hold.
pub const PRACK_HAS_A_LEG: &str = "prack-has-a-leg";
/// An owed PRACK names a dialog its leg does not hold.
pub const PRACK_HAS_A_DIALOG: &str = "prack-has-a-dialog";
/// A machine-bound rule moved its own cursor along an edge it does not
/// declare (ADR-0016 X1).
pub const UNDECLARED_TRANSITION: &str = "rule-declares-its-transitions";
/// A machine-bound rule emitted a side effect it does not declare (ADR-0016 X9).
pub const UNDECLARED_EFFECT: &str = "rule-declares-its-effects";
/// A lifecycle handler took an action outside call ext, its own cursor and
/// its own timers; the action is dropped.
pub const LIFECYCLE_OUT_OF_SCOPE: &str = "lifecycle-handler-keeps-its-scope";
/// The call's record does not rebuild the caller's INVITE a step read
/// (ADR-0052 §11): the turn's call is unrecoverable.
pub const CALLER_INVITE_REBUILDS: &str = "caller-invite-rebuilds";
/// A call reached `Terminated` with no termination record naming its cause.
pub const TERMINATION_RECORDED: &str = "terminal-call-records-its-termination";
/// A `Keepalive` was armed beyond one keepalive interval; it is held to one.
pub const KEEPALIVE_WITHIN_CADENCE: &str = "keepalive-armed-within-one-cadence";
/// The leg an in-dialog request was being built for left the record while
/// it was built; the request is not sent.
pub const REQUEST_KEEPS_ITS_LEG: &str = "request-keeps-its-leg";
/// Text stated from outside the stack did not fit its place on the wire and
/// was left out of the message
/// ([`crate::effects::BufferedObservabilityEffect::StatedTextSkipped`]).
pub const STATED_TEXT_FITS: &str = "stated-text-fits-its-position";
/// A step read a stored image of the call back and it did not read: a stack
/// defect or a damaged replication frame ([`sip_message::read_stored`]). The
/// step does nothing; the call goes on.
pub const STORED_IMAGE_READS: &str = "stored-image-reads";

/// A rule armed a timer type only the framework arms (`Rung`,
/// `NotifyAnswerCap`, `HeldAnswer`); the arm is dropped (ADR-0057 §1).
pub const FRAMEWORK_TIMER: &str = "framework-arms-its-own-timers";

/// Every defect a turn reports, by name.
pub const DEFECTS: [&str; 12] = [
    PRACK_HAS_A_LEG,
    PRACK_HAS_A_DIALOG,
    UNDECLARED_TRANSITION,
    UNDECLARED_EFFECT,
    LIFECYCLE_OUT_OF_SCOPE,
    CALLER_INVITE_REBUILDS,
    TERMINATION_RECORDED,
    KEEPALIVE_WITHIN_CADENCE,
    REQUEST_KEEPS_ITS_LEG,
    STATED_TEXT_FITS,
    STORED_IMAGE_READS,
    FRAMEWORK_TIMER,
];

/// A defect a turn met mid-way: a step that found the record unable to carry
/// what it must do, or a rule acting outside what it declared. The call is the
/// record the step read.
#[derive(Debug, Clone)]
pub struct Defect {
    pub name: &'static str,
    pub detail: String,
    pub call: Box<Call>,
}

/// What a turn has reported and the shell has not drained yet.
#[derive(Clone, Debug, Default)]
pub struct TurnNotes {
    trace: Vec<TraceFact>,
    repeat_give_ups: Vec<&'static str>,
    message_caps: u32,
    caps_near: Vec<Cap>,
    cap_readings: CapReadings,
    defects: Vec<Defect>,
}

/// What a landed turn left its call holding against each cap, the largest
/// reading of each: the measurement the caps' defaults are set from
/// (ADR-0057 §9). A cap on what grows with a call's age is also read per
/// started hour of the call's granted duration.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CapReadings {
    values: [u64; Cap::ALL.len()],
    per_hour: [u64; Cap::ALL.len()],
}

impl CapReadings {
    /// Raise `cap`'s slot of `slots` to `value`.
    fn raise(slots: &mut [u64; Cap::ALL.len()], cap: Cap, value: u64) {
        let at = Cap::ALL.iter().position(|c| *c == cap);
        if let Some(slot) = at.and_then(|at| slots.get_mut(at)) {
            *slot = (*slot).max(value);
        }
    }

    /// Each cap read, with its reading and its reading per started hour
    /// (`0`: none taken).
    pub fn iter(&self) -> impl Iterator<Item = (Cap, u64, u64)> + '_ {
        Cap::ALL
            .into_iter()
            .zip(self.values.iter().zip(self.per_hour.iter()))
            .map(|(cap, (value, per_hour))| (cap, *value, *per_hour))
    }
}

impl TurnNotes {
    /// A trace fact of `call` of `kind` at `at_ms`, `detail` rendered only for
    /// a sampled call: an unsampled call builds nothing.
    pub fn trace(
        &mut self,
        call: &Call,
        kind: &'static str,
        at_ms: i64,
        detail: impl FnOnce() -> String,
    ) {
        if sampled(call) {
            self.trace.push(TraceFact { kind, at_ms, detail: detail() });
        }
    }

    /// A dialog-level ladder of obligation `kind` ran to its give-up.
    pub fn repeat_give_up(&mut self, kind: &'static str) {
        self.repeat_give_ups.push(kind);
    }

    /// The message cap tore a runaway call down.
    pub fn message_cap_terminated(&mut self) {
        self.message_caps = self.message_caps.saturating_add(1);
    }

    /// The turn grew the collection `cap` bounds across half its cap.
    pub fn cap_near(&mut self, cap: Cap) {
        self.caps_near.push(cap);
    }

    /// The turn landed leaving `value` against `cap`.
    pub fn cap_reading(&mut self, cap: Cap, value: u64) {
        CapReadings::raise(&mut self.cap_readings.values, cap, value);
    }

    /// The turn landed leaving `value` per started hour of the call's
    /// granted duration against `cap`.
    pub fn cap_reading_per_hour(&mut self, cap: Cap, value: u64) {
        CapReadings::raise(&mut self.cap_readings.per_hour, cap, value);
    }

    /// The defect `name`, with `detail`, met on `call`.
    pub fn defect(&mut self, name: &'static str, detail: String, call: &Call) {
        self.defects.push(Defect { name, detail, call: Box::new(call.clone()) });
    }

    /// Everything reported so far, leaving nothing behind.
    pub fn take(&mut self) -> TurnNotes {
        std::mem::take(self)
    }

    /// The trace facts, in the order the turn met them.
    pub fn trace_facts(&self) -> &[TraceFact] {
        &self.trace
    }

    /// The obligation kinds of the ladders that gave up, one per give-up.
    pub fn repeat_give_ups(&self) -> &[&'static str] {
        &self.repeat_give_ups
    }

    /// How many runaway calls the message cap tore down.
    pub fn message_caps(&self) -> u32 {
        self.message_caps
    }

    /// The caps whose half the turn crossed, one per crossing.
    pub fn caps_near(&self) -> &[Cap] {
        &self.caps_near
    }

    /// What the turn left its call holding against each cap.
    pub fn cap_readings(&self) -> &CapReadings {
        &self.cap_readings
    }

    /// The defects, in the order the turn met them.
    pub fn defects(&self) -> &[Defect] {
        &self.defects
    }

    /// Whether the turn met a record that does not rebuild its caller's
    /// INVITE ([`CALLER_INVITE_REBUILDS`]): the shell drops the turn's result
    /// and discharges the call silently.
    pub fn unrecoverable(&self) -> bool {
        self.defects.iter().any(|d| d.name == CALLER_INVITE_REBUILDS)
    }
}
