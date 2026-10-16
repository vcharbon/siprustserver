//! Result classification — how a finished load call is bucketed for counting
//! and for the bounded per-class callflow samples.
//!
//! Three layers: [`CallOutcome`] is the raw result of one call, read from the
//! scenario's verdict (an accepted branch, a structured [`StepError`] at its
//! [`FailedAt`], or a caught panic). [`ResultClass`] collapses it to a
//! low-cardinality bucket key (e.g. `status_503`, `timeout`, `panic`) so the
//! Prometheus `class` label stays small; every accepted branch is `ok`.
//! [`CallOutcome::case`] refines the class into a still-bounded *case*
//! discriminator (which RFC rule fired, which check failed, which step a call
//! failed at) so the first-N sample capture keeps distinct failure modes apart
//! instead of filling one `rfc_audit_fail` bucket with N copies of the first
//! rule to fire.

use scenario_harness::actor::{FailedAt, ScenarioVerdict};
use scenario_harness::StepError;

/// The raw outcome of one load call, before bucketing.
#[derive(Debug, Clone)]
pub enum CallOutcome {
    /// The call ended on one of its scenario's accepted branches.
    Ok { branch: &'static str },
    /// The call failed at `at` (the agent, its pending goal, the pending
    /// barrier) because of `cause`.
    Step { at: FailedAt, cause: StepError },
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

    /// Whether this class is not a failure: the call ended on an accepted
    /// branch. Drives the OK/NOK split of the report and index and the clean
    /// release of a from-user key.
    pub fn is_ok(&self) -> bool {
        matches!(self, ResultClass::Ok)
    }

    /// [`is_ok`](Self::is_ok) of the class whose [`label`](Self::label) is
    /// `label`.
    pub fn label_is_ok(label: &str) -> bool {
        label == "ok"
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
            CallOutcome::Ok { .. } => ResultClass::Ok,
            CallOutcome::RfcAuditFail(_) => ResultClass::RfcAuditFail,
            CallOutcome::CheckFail(_) => ResultClass::CheckFail,
            CallOutcome::Panic(_) => ResultClass::Panic,
            CallOutcome::Rejected(_) => ResultClass::Rejected,
            CallOutcome::Step { cause, .. } => match cause {
                StepError::Timeout { .. } | StepError::QueueClosed { .. } => ResultClass::Timeout,
                StepError::WrongStatus { got, .. } => ResultClass::WrongStatus(*got),
                StepError::WrongMethod { .. } => ResultClass::WrongMethod,
                StepError::UnexpectedKind { .. } => ResultClass::Unexpected,
                StepError::Transport { .. } => ResultClass::Transport,
                StepError::Unparseable { .. } => ResultClass::Unparseable,
                // A request a UA's dialog table refuses is an RFC 3261 §12.2.2
                // violation of whoever sent it.
                StepError::Dialog { .. } => ResultClass::RfcAuditFail,
            },
        }
    }
}

impl From<ScenarioVerdict> for CallOutcome {
    fn from(v: ScenarioVerdict) -> Self {
        match v {
            ScenarioVerdict::Success { branch } => CallOutcome::Ok { branch },
            ScenarioVerdict::Failure { at, cause } => CallOutcome::Step { at, cause },
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
        matches!(self, CallOutcome::Ok { .. })
    }

    /// A human-readable one-line detail for the error sample (None for Ok).
    pub fn detail(&self) -> Option<String> {
        match self {
            CallOutcome::Ok { .. } => None,
            CallOutcome::Step { at, cause } => Some(format!("{cause} (at {at})")),
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
    /// (a different RFC rule, a different failed check, a different failed
    /// step) gets its own sample bucket. Empty for Ok.
    ///
    /// Cardinality stays structural: RFC rule ids and check `<on>.<field>`
    /// selectors are finite authored sets; a failed step ([`FailedAt`]) is
    /// static agent, goal and barrier names. Free-form text (finding details,
    /// panic messages — they embed Call-IDs/branches) is deliberately NEVER
    /// keyed. `last_phase` is the call's last reached lifecycle phase — the
    /// "where in the callflow" axis of a panic; the post-hoc oracles (RFC
    /// audit, checks) run on completed calls, where the rule/check id already
    /// localises the offence.
    pub fn case(&self, last_phase: Option<&'static str>) -> String {
        let case = match self {
            CallOutcome::Ok { .. } => String::new(),
            CallOutcome::Step { at, .. } => at.to_string(),
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

    fn wrong_status(got: u16) -> CallOutcome {
        CallOutcome::Step {
            at: FailedAt { who: "alice", goal: Some(("bye", 1)), barrier: "established" },
            cause: StepError::WrongStatus {
                who: "alice".into(),
                expected: 180,
                got,
                reason: "Busy Here".into(),
            },
        }
    }

    /// Every accepted branch classes `ok`; a failure classes by its cause and
    /// is keyed by the step it failed at.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn a_branch_is_ok_and_a_failure_is_keyed_by_its_step() {
        for branch in ["answered", "rejected", "refused_overload"] {
            let ok = CallOutcome::Ok { branch };
            assert_eq!(ResultClass::from(&ok).label(), "ok", "{branch}");
            assert_eq!(ok.case(Some("connected")), "", "{branch}");
            assert!(ok.is_checkable());
        }
        let failed = wrong_status(503);
        let class = ResultClass::from(&failed);
        assert_eq!(class.label(), "status_503");
        assert!(!class.is_ok());
        assert!(!ResultClass::label_is_ok("status_503"));
        assert_eq!(failed.case(Some("connected")), "alice-bye-1@established");
        assert!(!failed.is_checkable());
    }

    /// An accepted branch is still audited, so an RFC-dirty reject is
    /// `rfc_audit_fail`; a failure is not audited.
    #[test]
    #[ignore = "slow lane: loadgen"]
    fn an_accepted_branch_is_audited_and_checked() {
        let finding = sip_net::RfcFinding {
            rule: "unacked-invite-non-2xx-final".to_string(),
            lane: "10.0.0.1:5060".to_string(),
            detail: "reject never ACKed".to_string(),
            advisory: false,
            offending: None,
            charged: None,
        };
        let rejected = || CallOutcome::Ok { branch: "rejected" };
        let dirty = rejected().audited(|| vec![finding.clone()]);
        assert_eq!(ResultClass::from(&dirty), ResultClass::RfcAuditFail);
        let clean = rejected().audited(Vec::new);
        assert!(matches!(clean, CallOutcome::Ok { branch: "rejected" }));
        let failed = wrong_status(503).audited(|| panic!("a failure is not audited"));
        assert!(!failed.is_checkable());
    }
}
