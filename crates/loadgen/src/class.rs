//! Result classification — how a finished load call is bucketed for counting
//! and for the bounded per-class callflow samples.
//!
//! Three layers: [`CallOutcome`] is the raw result of one call (Ok, a
//! structured [`StepError`], or a caught panic). [`ResultClass`] collapses it
//! to a low-cardinality bucket key (e.g. `status_503`, `timeout`, `panic`) so
//! the Prometheus `class` label stays small. A reject the call's shape
//! expects ([`CallOutcome::expecting`]) is its own non-failure class,
//! `expected_reject`. [`CallOutcome::case`] refines the
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
    /// The call ended on the reject status its shape declares as an expected
    /// outcome (carries the status).
    ExpectedReject(u16),
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
    /// The call was refused before any datagram: its correlation key is
    /// missing, unusable, held by a concurrent call, or cooling after a failed
    /// call. Carries the bounded reason (`no_from`, `userless_from`,
    /// `key_in_flight`, `key_cooling`).
    Rejected(&'static str),
}

/// A low-cardinality bucket for a call result. `Display`/[`label`](Self::label)
/// is the stable string used as the Prometheus `class` label and the sample
/// directory name.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ResultClass {
    Ok,
    /// The shape's expected reject: an outcome, not a failure.
    ExpectedReject,
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
    /// Refused before any datagram: the call's correlation key is missing,
    /// unusable, or already held by a concurrent call.
    Rejected,
    Panic,
}

impl ResultClass {
    /// The stable label string (Prometheus `class` value + sample dir name).
    pub fn label(&self) -> String {
        match self {
            ResultClass::Ok => "ok".to_string(),
            ResultClass::ExpectedReject => "expected_reject".to_string(),
            ResultClass::Timeout => "timeout".to_string(),
            ResultClass::WrongStatus(c) => format!("status_{c}"),
            ResultClass::WrongMethod => "wrong_method".to_string(),
            ResultClass::Unexpected => "unexpected".to_string(),
            ResultClass::Transport => "transport".to_string(),
            ResultClass::Unparseable => "unparseable".to_string(),
            ResultClass::RfcAuditFail => "rfc_audit_fail".to_string(),
            ResultClass::CheckFail => "check_fail".to_string(),
            ResultClass::Rejected => "rejected".to_string(),
            ResultClass::Panic => "panic".to_string(),
        }
    }

    /// Whether this class is not a failure: the happy path or the shape's
    /// expected reject. Drives the OK/NOK split of the report and index and
    /// the clean release of a from-user key.
    pub fn is_ok(&self) -> bool {
        matches!(self, ResultClass::Ok | ResultClass::ExpectedReject)
    }

    /// [`is_ok`](Self::is_ok) of the class whose [`label`](Self::label) is
    /// `label`.
    pub fn label_is_ok(label: &str) -> bool {
        label == ResultClass::Ok.label() || label == ResultClass::ExpectedReject.label()
    }

    /// Whether a failure of this class may be auto-excused as `chaos="near"`
    /// (acceptable kill collateral) when the **per-phase** rule also holds (a
    /// dialog-state transition occurred within the phase tolerance of the fault).
    ///
    /// The accepted constraint: *a call whose dialog state changed within
    /// ~200 ms of the kill may take a small impact — established and ringing
    /// calls are what we protect.* So a SIP **protocol** symptom of a
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
    /// code panic), `Unparseable` (wire corruption) and `Rejected` (a key
    /// refused before any datagram). `CheckFail` IS excusable
    /// like the other protocol/content symptoms — a call rerouted mid-kill can
    /// legitimately show a different wire shape than the case's oracle expects.
    pub fn chaos_excusable(&self) -> bool {
        !matches!(self, ResultClass::Panic | ResultClass::Unparseable | ResultClass::Rejected)
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
            CallOutcome::ExpectedReject(_) => ResultClass::ExpectedReject,
            CallOutcome::RfcAuditFail(_) => ResultClass::RfcAuditFail,
            CallOutcome::CheckFail(_) => ResultClass::CheckFail,
            CallOutcome::Panic(_) => ResultClass::Panic,
            CallOutcome::Rejected(_) => ResultClass::Rejected,
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
    /// This outcome after the sampled RFC audit: `audit` runs on an outcome
    /// that is no failure, and any finding it returns makes the call
    /// [`CallOutcome::RfcAuditFail`] (carried structured, so the report
    /// buckets samples by rule id).
    pub fn audited(self, audit: impl FnOnce() -> Vec<sip_net::RfcFinding>) -> Self {
        if !self.is_checkable() {
            return self;
        }
        let findings = audit();
        if findings.is_empty() {
            self
        } else {
            CallOutcome::RfcAuditFail(findings)
        }
    }

    /// Whether an attached case's checks judge this outcome: one that is no
    /// failure (a failed or RFC-dirty call already explains itself).
    pub fn is_checkable(&self) -> bool {
        matches!(self, CallOutcome::Ok | CallOutcome::ExpectedReject(_))
    }

    /// This outcome under the call's declared expected reject status: the
    /// `caller` agent receiving exactly that status on its initial INVITE —
    /// before the call reached `connected` — becomes
    /// [`CallOutcome::ExpectedReject`]. Any other outcome, or that status on
    /// another agent or after connect (a re-INVITE, BYE, REFER), is unchanged.
    pub fn expecting(self, expected_reject: Option<u16>, caller: &str, connected: bool) -> Self {
        match (&self, expected_reject) {
            (CallOutcome::Step(StepError::WrongStatus { who, got, .. }), Some(code))
                if *got == code && who == caller && !connected =>
            {
                CallOutcome::ExpectedReject(code)
            }
            _ => self,
        }
    }

    /// Whether this outcome may be excused as chaos collateral:
    /// [`ResultClass::chaos_excusable`], except that a key-contention rejection
    /// is excusable — `key_in_flight` (the holder may be a call a fault keeps
    /// open) and `key_cooling` (the previous call may have failed on a fault) —
    /// while a missing or unusable key is not.
    pub fn chaos_excusable(&self) -> bool {
        match self {
            CallOutcome::Rejected(reason) => matches!(*reason, "key_in_flight" | "key_cooling"),
            other => ResultClass::from(other).chaos_excusable(),
        }
    }

    /// A human-readable one-line detail for the error sample (None for Ok).
    pub fn detail(&self) -> Option<String> {
        match self {
            CallOutcome::Ok => None,
            CallOutcome::ExpectedReject(code) => Some(format!("expected reject {code}")),
            CallOutcome::Step(e) => Some(e.to_string()),
            CallOutcome::Panic(m) => Some(format!("panic: {m}")),
            CallOutcome::Rejected(reason) => {
                Some(format!("rejected before any datagram: {reason}"))
            }
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
    /// gets its own sample bucket. Empty for Ok; the status for an expected
    /// reject.
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
            CallOutcome::ExpectedReject(code) => code.to_string(),
            CallOutcome::Step(e) => {
                format!("{}@{}", step_who(e), last_phase.unwrap_or("start"))
            }
            CallOutcome::Panic(_) => last_phase.unwrap_or("start").to_string(),
            CallOutcome::Rejected(reason) => reason.to_string(),
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
    let distinct: std::collections::BTreeSet<String> = ids.into_iter().map(Into::into).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A key held by a concurrent call, or cooling after a failed one, may be
    /// a fault's doing, so those rejections are chaos-excusable; a missing or
    /// unusable key is not.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn only_key_contention_rejections_are_chaos_excusable() {
        for reason in ["key_in_flight", "key_cooling"] {
            assert!(CallOutcome::Rejected(reason).chaos_excusable(), "{reason}");
        }
        for reason in ["no_from", "userless_from"] {
            assert!(!CallOutcome::Rejected(reason).chaos_excusable(), "{reason}");
        }
        assert!(!CallOutcome::Panic("p".into()).chaos_excusable());
        assert!(CallOutcome::Step(StepError::Timeout { who: "alice".into() }).chaos_excusable());
    }

    fn wrong_status(got: u16) -> CallOutcome {
        CallOutcome::Step(StepError::WrongStatus {
            who: "alice".into(),
            expected: 200,
            got,
            reason: "Busy Here".into(),
        })
    }

    /// The shape's expected reject status classes `expected_reject`, a
    /// non-failure keyed by its status; another status, or the same one with
    /// no reject declared, stays a `status_<code>` failure.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn the_expected_reject_is_its_own_non_failure_class() {
        let expected = wrong_status(486).expecting(Some(486), "alice", false);
        let class = ResultClass::from(&expected);
        assert_eq!(class.label(), "expected_reject");
        assert!(class.is_ok());
        assert!(ResultClass::label_is_ok("expected_reject"));
        assert!(expected.chaos_excusable());
        assert_eq!(expected.case(Some("start")), "486");

        let refused = ResultClass::from(&wrong_status(503).expecting(Some(486), "alice", false));
        assert_eq!(refused.label(), "status_503");
        assert!(!refused.is_ok());
        assert!(!ResultClass::label_is_ok("status_503"));

        let undeclared = ResultClass::from(&wrong_status(486).expecting(None, "alice", false));
        assert_eq!(undeclared.label(), "status_486");
        assert!(!undeclared.is_ok());
        assert!(matches!(CallOutcome::Ok.expecting(Some(486), "alice", false), CallOutcome::Ok));
    }

    /// A reject status seen by another agent than the caller (a callee's
    /// answer to its own BYE or REFER), or by the caller once the call
    /// connected (a re-INVITE, a BYE), is no expected reject.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn only_the_callers_reject_is_expected() {
        let busy = |who: &str| {
            CallOutcome::Step(StepError::WrongStatus {
                who: who.into(),
                expected: 200,
                got: 486,
                reason: "Busy Here".into(),
            })
        };
        let class = |o: CallOutcome| ResultClass::from(&o).label();
        assert_eq!(class(busy("bob").expecting(Some(486), "alice", false)), "status_486");
        assert_eq!(class(busy("alice").expecting(Some(486), "alice", true)), "status_486");
        assert_eq!(class(busy("alice").expecting(Some(486), "alice", false)), "expected_reject");
    }

    /// An expected reject is no failure, so the sampled RFC audit and the
    /// case's checks judge it like an ok call; a failure is not audited.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn an_expected_reject_is_audited_and_checked_like_ok() {
        let finding = sip_net::RfcFinding {
            rule: "unacked-invite-non-2xx-final".to_string(),
            lane: "10.0.0.1:5060".to_string(),
            detail: "reject never ACKed".to_string(),
            advisory: false,
            offending: None,
            charged: None,
        };
        let dirty = CallOutcome::ExpectedReject(486).audited(|| vec![finding.clone()]);
        assert_eq!(ResultClass::from(&dirty), ResultClass::RfcAuditFail);
        let clean = CallOutcome::ExpectedReject(486).audited(Vec::new);
        assert!(matches!(clean, CallOutcome::ExpectedReject(486)));
        assert!(clean.is_checkable());
        let failed = wrong_status(503).audited(|| panic!("a failure is not audited"));
        assert!(!failed.is_checkable());
    }
}
