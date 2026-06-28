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
            ResultClass::Panic => "panic".to_string(),
        }
    }

    /// Whether this class is a success (drives the OK/NOK split in the report).
    pub fn is_ok(&self) -> bool {
        matches!(self, ResultClass::Ok)
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
        }
    }
}
