//! Result classification — how a finished load call is bucketed for counting
//! and for the bounded per-class callflow samples.
//!
//! Two layers: [`CallOutcome`] is the raw result of one call (Ok, a structured
//! [`StepError`], or a caught panic). [`ResultClass`] collapses it to a
//! low-cardinality bucket key (e.g. `status_486`, `timeout`, `panic`) so the
//! sample store and the Prometheus `class` label stay small.

use scenario_harness::StepError;

/// The raw outcome of one load call, before bucketing.
#[derive(Debug, Clone)]
pub enum CallOutcome {
    /// The scenario completed its happy path.
    Ok,
    /// A `try_*` step returned a structured failure.
    Step(StepError),
    /// The scenario future panicked (caught at the per-call `catch_unwind`
    /// boundary); the string is the panic message, best-effort.
    Panic(String),
    /// The call otherwise succeeded but its sampled trace failed the RFC audit.
    RfcAuditFail(String),
    /// The call otherwise succeeded but its attached Test case's checks failed
    /// over the sampled trace (the e2e check engine's verdicts); the string
    /// lists the failed checks. Sampled calls only — checks are a per-sample
    /// oracle, like the RFC audit.
    CheckFail(String),
}

/// A low-cardinality bucket for a call result. `Display`/[`label`](Self::label)
/// is the stable string used as the Prometheus `class` label and the sample
/// directory name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResultClass {
    Ok,
    Timeout,
    /// Wrong response status — carries the code so e.g. a 486 and a 503 are
    /// distinct buckets (bounded cardinality: SIP status codes).
    WrongStatus(u16),
    WrongMethod,
    /// A request arrived where a response was expected, or vice-versa.
    Unexpected,
    Transport,
    Unparseable,
    RfcAuditFail,
    CheckFail,
    Panic,
}

impl ResultClass {
    /// The stable label string (Prometheus `class` value + sample dir name).
    pub fn label(&self) -> String {
        match self {
            ResultClass::Ok => "ok".to_string(),
            ResultClass::Timeout => "timeout".to_string(),
            ResultClass::WrongStatus(c) => format!("status_{c}"),
            ResultClass::WrongMethod => "wrong_method".to_string(),
            ResultClass::Unexpected => "unexpected".to_string(),
            ResultClass::Transport => "transport".to_string(),
            ResultClass::Unparseable => "unparseable".to_string(),
            ResultClass::RfcAuditFail => "rfc_audit_fail".to_string(),
            ResultClass::CheckFail => "check_fail".to_string(),
            ResultClass::Panic => "panic".to_string(),
        }
    }

    /// Whether this class is a success (drives the OK/NOK split in the report).
    pub fn is_ok(&self) -> bool {
        matches!(self, ResultClass::Ok)
    }

    /// Whether a failure of this class may be auto-excused as `chaos="near"`
    /// (acceptable kill collateral) when the **per-phase** rule also holds (a
    /// dialog-state transition occurred within the phase tolerance of the fault).
    ///
    /// The accepted constraint (2026-06-29): *a call whose dialog state changed
    /// within ~200 ms of the kill may take a small impact — established and
    /// ringing calls are what we protect.* So a SIP **protocol** symptom of a
    /// concurrent-with-the-kill state change (a `RfcAuditFail` CSeq desync, a
    /// `WrongMethod` phantom CANCEL, an `Unexpected` 481) IS excusable — those are
    /// exactly the forked-b-leg confirm-race collateral, which only ever hits a
    /// call confirming *at* the kill (established calls flushed their state and
    /// reclaim clean). The per-phase classifier gates it on the near-kill
    /// transition, so a *stably-established* call that fails far from any kill
    /// still lands in `clear`.
    ///
    /// Only the **timing-independent** classes are never excused, because their
    /// cause is unrelated to dialog timing and should always be seen: `Panic` (a
    /// code panic) and `Unparseable` (wire corruption). `CheckFail` IS excusable
    /// like the other protocol/content symptoms — a call rerouted mid-kill can
    /// legitimately show a different wire shape than the case's oracle expects.
    pub fn chaos_excusable(&self) -> bool {
        !matches!(self, ResultClass::Panic | ResultClass::Unparseable)
    }
}

impl std::fmt::Display for ResultClass {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

impl From<&CallOutcome> for ResultClass {
    fn from(o: &CallOutcome) -> Self {
        match o {
            CallOutcome::Ok => ResultClass::Ok,
            CallOutcome::RfcAuditFail(_) => ResultClass::RfcAuditFail,
            CallOutcome::CheckFail(_) => ResultClass::CheckFail,
            CallOutcome::Panic(_) => ResultClass::Panic,
            CallOutcome::Step(e) => match e {
                StepError::Timeout { .. } | StepError::QueueClosed { .. } => ResultClass::Timeout,
                StepError::WrongStatus { got, .. } => ResultClass::WrongStatus(*got),
                StepError::WrongMethod { .. } => ResultClass::WrongMethod,
                StepError::UnexpectedKind { .. } => ResultClass::Unexpected,
                StepError::Transport { .. } => ResultClass::Transport,
                StepError::Unparseable { .. } => ResultClass::Unparseable,
            },
        }
    }
}

impl CallOutcome {
    /// A human-readable one-line detail for the error sample (None for Ok).
    pub fn detail(&self) -> Option<String> {
        match self {
            CallOutcome::Ok => None,
            CallOutcome::Step(e) => Some(e.to_string()),
            CallOutcome::Panic(m) => Some(format!("panic: {m}")),
            CallOutcome::RfcAuditFail(d) => Some(format!("rfc audit: {d}")),
            CallOutcome::CheckFail(d) => Some(format!("case checks: {d}")),
        }
    }
}
