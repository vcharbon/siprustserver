//! Result classification — how a finished load call is bucketed for counting
//! and for the bounded per-class callflow samples.
//!
//! Three layers: [`CallOutcome`] is the raw result of one call (Ok, a
//! structured [`StepError`], or a caught panic). [`ResultClass`] collapses it
//! to a low-cardinality bucket key (e.g. `status_486`, `timeout`, `panic`) so
//! the Prometheus `class` label stays small. [`CallOutcome::case`] refines the
//! class into a still-bounded *case* discriminator (which RFC rule fired, which
//! check failed, which agent/phase a step died at) so the first-N sample
//! capture keeps distinct failure modes apart instead of filling one
//! `rfc_audit_fail` bucket with N copies of the first rule to fire.

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
    /// The call otherwise succeeded but its sampled trace failed the RFC audit
    /// — carries the structured findings so the case key can bucket by rule id
    /// (the joined human detail is derived in [`detail`](Self::detail)).
    RfcAuditFail(Vec<sip_net::RfcFinding>),
    /// The call otherwise succeeded but its attached Test case's checks failed
    /// over the sampled trace — carries the FAILED verdicts only. Sampled calls
    /// only — checks are a per-sample oracle, like the RFC audit.
    CheckFail(Vec<e2e_model::CheckVerdict>),
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
            CallOutcome::RfcAuditFail(findings) => Some(format!(
                "rfc audit: {}",
                findings.iter().map(|f| f.detail.clone()).collect::<Vec<_>>().join("; ")
            )),
            CallOutcome::CheckFail(failed) => Some(format!(
                "case checks: {}",
                failed
                    .iter()
                    .map(|v| format!("{} {}: {}", v.on, v.field, v.detail))
                    .collect::<Vec<_>>()
                    .join("; ")
            )),
        }
    }

    /// The bounded **case** discriminator refining [`ResultClass`] for the
    /// first-N sample capture: same scenario + same class but a different case
    /// (a different RFC rule, a different failed check, a different agent/phase)
    /// gets its own sample bucket. Empty for Ok (and any un-refined outcome).
    ///
    /// Cardinality stays structural: RFC rule ids and check `<on>.<field>`
    /// selectors are finite authored sets; agent names and lifecycle phase
    /// names are static strings. Free-form text (finding details, panic
    /// messages — they embed Call-IDs/branches) is deliberately NEVER keyed.
    /// `last_phase` is the call's last reached lifecycle phase — the
    /// "where in the callflow" axis for mid-flow deaths (steps/panics); the
    /// post-hoc oracles (RFC audit, checks) run on completed calls, where the
    /// rule/check id already localises the offence.
    pub fn case(&self, last_phase: Option<&'static str>) -> String {
        let case = match self {
            CallOutcome::Ok => String::new(),
            CallOutcome::Step(e) => {
                format!("{}@{}", step_who(e), last_phase.unwrap_or("start"))
            }
            CallOutcome::Panic(_) => last_phase.unwrap_or("start").to_string(),
            CallOutcome::RfcAuditFail(findings) => {
                joined_distinct(findings.iter().map(|f| f.rule.as_str()))
            }
            CallOutcome::CheckFail(failed) => {
                joined_distinct(failed.iter().map(|v| format!("{}.{}", v.on, v.field)))
            }
        };
        slug(&case)
    }
}

/// The agent a step failure is attributed to (the `who` every [`StepError`]
/// variant carries) — bounded: load agents are named by role (`alice`, `bob`,
/// `callee`, …).
fn step_who(e: &StepError) -> &str {
    match e {
        StepError::Timeout { who }
        | StepError::QueueClosed { who }
        | StepError::Unparseable { who, .. }
        | StepError::WrongStatus { who, .. }
        | StepError::WrongMethod { who, .. }
        | StepError::UnexpectedKind { who, .. }
        | StepError::Transport { who, .. } => who,
    }
}

/// Sorted-distinct ids joined with `+`, capped at 3 (`+{n}` names the overflow)
/// so a many-findings call can't mint an unbounded key or an absurd dir name.
fn joined_distinct<I: IntoIterator<Item = impl Into<String>>>(ids: I) -> String {
    let distinct: std::collections::BTreeSet<String> =
        ids.into_iter().map(Into::into).collect();
    let n = distinct.len();
    let mut out: Vec<String> = distinct.into_iter().take(3).collect();
    if n > 3 {
        out.push(format!("+{}", n - 3));
    }
    out.join("+")
}

/// Make a case key filesystem/URL-safe (it becomes a sample directory segment):
/// keep `[A-Za-z0-9._+@-]`, map everything else to `-`.
fn slug(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '@' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}
