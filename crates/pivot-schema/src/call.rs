//! The `calls` block (`PCAP2TEST_PIVOT_V3.md` §4): one entry per call the
//! document plays, each owning its attempt chain and the routing configuration
//! detected for it.
//!
//! The driver COMPILES this block into whatever a target lane needs before a
//! run; the interpreter never interprets it. Parallel forks are same-`position`
//! attempts on different `branch`es; a sequential hunt is one branch with
//! several positions. A captured case is exactly one call; a concurrency test
//! is several, and their flows interleave through step ids.

use std::fmt;
use std::str::FromStr;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One call: who places it, where it is routed, and how the system handled
/// provisionals for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Call {
    /// Unique within the document. Tier-2 position tokens qualify with it
    /// (`<call-id>.called[b][s]`) once a document declares more than one call.
    pub id: String,
    /// The leg that originates the call.
    pub caller_leg: String,
    /// The attempt chain, in one encoding. Empty exactly when `refused` or
    /// `abandoned` says why the call dialed nobody.
    pub attempts: Vec<Attempt>,
    /// The routing decision REFUSED the call: the platform dialed nobody and
    /// answered the caller itself. Exclusive with a chain — a call either has
    /// attempts or has none because it was refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refused: Option<Refused>,
    /// The CALLER abandoned the call before any called leg crossed this
    /// vantage. Exclusive with a chain and with `refused`: the platform took no
    /// routing decision here, so nothing it sent the caller states one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub abandoned: Option<Abandoned>,
    /// The provisional-handling profile the captured system ran for THIS call.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relay18x: Option<Relay18x>,
}

/// A call the routing decision refused before any dial.
///
/// The final itself is NOT restated here: `step` names the flow step carrying
/// it, so the status, the reason phrase and the headers the caller got have one
/// home and cannot disagree with the flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Refused {
    /// The caller-leg step carrying the failure final the platform answered
    /// the call with.
    pub step: String,
    /// Why the cut reads this vantage as a refusal rather than as a dial it
    /// lost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// A call the CALLER abandoned before any dial crossed this vantage.
///
/// The CANCEL itself is NOT restated here: `step` names the flow step carrying
/// it, so what the caller sent and when it sent it has one home. The final that
/// follows a cancelled INVITE is the `487` RFC 3261 §9.2 owes it and states no
/// routing decision, which is why an abandon is not a [`Refused`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Abandoned {
    /// The caller-leg step carrying the CANCEL the caller SENT.
    pub step: String,
    /// Why the cut reads this vantage as an abandon rather than as a decision
    /// the platform took.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
}

/// One dialed attempt in a call's chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    /// Parallel branch index.
    pub branch: u32,
    /// Position within the branch's sequential chain. Explicit, not array
    /// order, so `(branch, position)` stays a stable key under sorted-key
    /// formatting.
    pub position: u32,
    /// The leg id the flow uses for this attempt.
    pub leg: String,
    /// Who the attempt dials.
    pub callee: Callee,
    /// The attempt's own terminal INVITE final at its vantage. What makes the
    /// chain barrier derivable without a per-step ordering field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub r#final: Option<Final>,
    /// Why the platform left this attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Cause>,
    /// What ADDED this leg to the call: a transfer, or a media resource
    /// inserted into it. A joined leg is a parallel arrival, not a failure —
    /// the first attempt answered and the call then dialed somebody else — so
    /// it is exclusive with the failure-only sequential `cause`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub joined_by: Option<JoinedBy>,
    /// The captured signals that justify `cause`. Belongs to the attempt that
    /// FAILED.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cause_evidence: Vec<String>,
    /// Why the correlator put THIS attempt in that chain. Belongs to the
    /// attempt that JOINED.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join_evidence: Option<String>,
    /// INVITE-to-CANCEL dwell, present exactly when `cause` is `no-answer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_answer_ms: Option<u64>,
}

/// Lowest and highest `no_answer_ms` a whole-second ring timer can have armed.
/// Below the floor nothing arms; above the ceiling a "ring" is a mis-cut span.
pub const NO_ANSWER_MS_BAND: std::ops::RangeInclusive<u64> = 1_000..=3_600_000;

impl Attempt {
    /// Whether `no_answer_ms` is stated exactly where it may be, and inside the
    /// armable band.
    pub fn no_answer_ms_is_declarable(&self) -> bool {
        match (&self.cause, self.no_answer_ms) {
            (Some(Cause::NoAnswer), Some(ms)) => NO_ANSWER_MS_BAND.contains(&ms),
            (Some(Cause::NoAnswer), None) | (_, Some(_)) => false,
            _ => true,
        }
    }
}

/// The dialed target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Callee {
    /// Name of an entry in the document's `identities` registry. The number
    /// itself is never embedded here: which number a name becomes is a per-lane
    /// binding the driver performs.
    pub identity: String,
}

/// What added a leg to a call that was already running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JoinedBy {
    /// Which mechanism joined the leg.
    pub kind: JoinKind,
    /// The flow step that performed the join — the REFER or INFO the platform
    /// accepted, or the request that inserted the media resource. It must be a step of a
    /// leg of the SAME call: a join is an event inside one call, and naming
    /// another call's step would make the chain unreadable.
    pub step: String,
}

/// The mechanisms that join a leg to a running call. Closed: each names a SIP
/// event the document itself carries as a step, and a mechanism nothing can
/// point at is not a join.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum JoinKind {
    /// A transfer: an accepted REFER dialed the transferee.
    Refer,
    /// A transfer: an INFO accepted as a transfer order dialed the transferee.
    Info,
    /// A media resource was inserted into the call.
    Mrf,
}

/// An attempt's terminal INVITE final at its own vantage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Final {
    /// The final response's status code.
    pub status: u16,
    /// When it arrived, in milliseconds from the case's first message.
    pub at_ms: u64,
}

/// Why the platform left an attempt. Closed vocabulary, every member read off a
/// captured datagram — and every member cites either the attempt's
/// DIALOG-CREATING final or an actual closer (BYE, CANCEL). An in-dialog final
/// never closes a leg, so it is `cause_evidence` and never a cause.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cause {
    /// The attempt rang and the platform's own timer gave up: a CANCEL closed
    /// it.
    NoAnswer,
    /// The callee answered busy.
    Busy,
    /// No final arrived before the transaction timed out.
    TransactionTimeout,
    /// A 3xx sent the platform elsewhere.
    Redirect(u16),
    /// A 4xx-6xx answered the attempt's own dialog-creating INVITE.
    External(u16),
    /// The platform released an ANSWERED attempt with a BYE. What made it send
    /// one is `cause_evidence` — a renegotiation the far end refused, a decision
    /// the platform took — because the closer is the datagram and the reason is
    /// the reading.
    ClosedBye,
}

impl fmt::Display for Cause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Cause::NoAnswer => f.write_str("no-answer"),
            Cause::Busy => f.write_str("busy"),
            Cause::TransactionTimeout => f.write_str("transaction-timeout"),
            Cause::ClosedBye => f.write_str("closed:bye"),
            Cause::Redirect(status) => write!(f, "redirect:{status}"),
            Cause::External(status) => write!(f, "external:{status}"),
        }
    }
}

impl FromStr for Cause {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "no-answer" => return Ok(Cause::NoAnswer),
            "busy" => return Ok(Cause::Busy),
            "transaction-timeout" => return Ok(Cause::TransactionTimeout),
            "closed:bye" => return Ok(Cause::ClosedBye),
            _ => {}
        }
        if let Some(status) = s.strip_prefix("redirect:") {
            return status_in(status, 300..=399).map(Cause::Redirect);
        }
        if let Some(status) = s.strip_prefix("external:") {
            return status_in(status, 400..=699).map(Cause::External);
        }
        Err(format!("cause {s:?} is not in the closed vocabulary"))
    }
}

fn status_in(text: &str, band: std::ops::RangeInclusive<u16>) -> Result<u16, String> {
    let status: u16 = text.parse().map_err(|_| format!("cause status {text:?} is not a number"))?;
    if band.contains(&status) {
        Ok(status)
    } else {
        Err(format!("cause status {status} is outside {}..={}", band.start(), band.end()))
    }
}

crate::string_token!(
    Cause,
    "Why the platform left an attempt: `no-answer`, `busy`, `transaction-timeout`, `closed:bye`, `redirect:<3xx>` or `external:<4xx-6xx>`.",
    "^(no-answer|busy|transaction-timeout|closed:bye|redirect:3[0-9]{2}|external:[4-6][0-9]{2})$"
);

/// The provisional-handling profile the CAPTURED system ran, in the routing
/// API's own vocabulary so a lane applies it without re-deciding. Every token
/// is open: these name a platform's configuration, not a SIP constant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Relay18x {
    /// Open profile token (how upstream `18x` are rewritten downstream).
    pub mode: String,
    /// Open token: how many upstream `18x` reach the caller.
    pub messages: String,
    /// Open prack-mode token, present only where the captured system answered
    /// 100rel itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prack: Option<String>,
    /// The detection signals that fired. Rides beside the fact, so a reviewer
    /// reads the evidence rather than the verdict alone.
    pub evidence: Vec<String>,
}

/// A tier-2 position token: `caller` or `called[branch][position]`, optionally
/// qualified by a call id (`c2.called[0][1]`) — required once a document
/// declares more than one call, since the bare form names one chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Position {
    /// The call the position belongs to, when the token qualifies it.
    pub call: Option<String>,
    /// Which party of that call the token names.
    pub role: Role,
}

/// Which party a [`Position`] names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The call's originator.
    Caller,
    /// The attempt at `(branch, position)` of the call's chain.
    Called { branch: u32, position: u32 },
}

impl Position {
    /// Parse a position token, or say why it is not one.
    pub fn parse(text: &str) -> Result<Self, String> {
        let (call, rest) = match text.split_once('.') {
            Some((call, rest)) if !call.is_empty() => (Some(call.to_string()), rest),
            _ => (None, text),
        };
        if rest == "caller" {
            return Ok(Position { call, role: Role::Caller });
        }
        let indices = rest
            .strip_prefix("called[")
            .and_then(|r| r.strip_suffix(']'))
            .and_then(|r| r.split_once("]["));
        match indices {
            Some((branch, position)) => {
                let branch = branch.parse().map_err(|_| bad(text))?;
                let position = position.parse().map_err(|_| bad(text))?;
                Ok(Position { call, role: Role::Called { branch, position } })
            }
            None => Err(bad(text)),
        }
    }
}

fn bad(text: &str) -> String {
    format!("position {text:?} is neither `caller` nor `called[b][s]`, qualified or bare")
}

impl fmt::Display for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(call) = &self.call {
            write!(f, "{call}.")?;
        }
        match self.role {
            Role::Caller => f.write_str("caller"),
            Role::Called { branch, position } => write!(f, "called[{branch}][{position}]"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_cause_round_trips_through_its_token() {
        for token in [
            "no-answer",
            "busy",
            "transaction-timeout",
            "closed:bye",
            "redirect:302",
            "external:486",
            "external:603",
        ] {
            assert_eq!(token.parse::<Cause>().unwrap().to_string(), token);
        }
    }

    #[test]
    fn a_parameterized_cause_refuses_a_status_outside_its_class() {
        assert!("redirect:404".parse::<Cause>().is_err());
        assert!("external:302".parse::<Cause>().is_err());
        assert!("external:700".parse::<Cause>().is_err());
        assert!("redirect:x".parse::<Cause>().is_err());
        assert!("abandoned".parse::<Cause>().is_err());
        // A closer is cited by name, not by the status of some other leg's final.
        assert!("closed:cancel".parse::<Cause>().is_err());
    }

    fn attempt(cause: Option<Cause>, no_answer_ms: Option<u64>) -> Attempt {
        Attempt {
            branch: 0,
            position: 0,
            leg: "B".into(),
            callee: Callee { identity: "called-0-0".into() },
            r#final: None,
            cause,
            joined_by: None,
            cause_evidence: Vec::new(),
            join_evidence: None,
            no_answer_ms,
        }
    }

    #[test]
    fn a_no_answer_dwell_is_declarable_only_with_its_cause_and_inside_the_band() {
        assert!(attempt(Some(Cause::NoAnswer), Some(15_139)).no_answer_ms_is_declarable());
        assert!(attempt(None, None).no_answer_ms_is_declarable());
        // Below the floor no whole-second timer armed; above the ceiling the span is mis-cut.
        assert!(!attempt(Some(Cause::NoAnswer), Some(92)).no_answer_ms_is_declarable());
        assert!(!attempt(Some(Cause::NoAnswer), Some(3_600_001)).no_answer_ms_is_declarable());
        // Stated without the cause, or the cause stated without the dwell.
        assert!(!attempt(Some(Cause::Busy), Some(15_139)).no_answer_ms_is_declarable());
        assert!(!attempt(Some(Cause::NoAnswer), None).no_answer_ms_is_declarable());
    }

    #[test]
    fn a_joined_leg_names_the_mechanism_and_the_step_that_joined_it() {
        let joined: JoinedBy = serde_json::from_str(r#"{"kind":"refer","step":"s8"}"#).unwrap();
        assert_eq!(joined.kind, JoinKind::Refer);
        let joined: JoinedBy = serde_json::from_str(r#"{"kind":"info","step":"s8"}"#).unwrap();
        assert_eq!(joined.kind, JoinKind::Info);
        assert_eq!(joined.step, "s8");
        assert_eq!(
            serde_json::to_string(&JoinedBy { kind: JoinKind::Mrf, step: "s3".into() }).unwrap(),
            r#"{"kind":"mrf","step":"s3"}"#
        );
    }

    #[test]
    fn a_join_mechanism_outside_the_vocabulary_is_refused() {
        assert!(serde_json::from_str::<JoinedBy>(r#"{"kind":"reroute","step":"s8"}"#).is_err());
        assert!(serde_json::from_str::<JoinedBy>(r#"{"kind":"refer"}"#).is_err());
        assert!(
            serde_json::from_str::<JoinedBy>(r#"{"kind":"refer","step":"s8","leg":"C"}"#).is_err()
        );
    }

    #[test]
    fn a_position_round_trips_bare_and_call_qualified() {
        for token in ["caller", "called[0][0]", "called[1][2]", "c2.caller", "c2.called[0][1]"] {
            assert_eq!(Position::parse(token).unwrap().to_string(), token);
        }
    }

    #[test]
    fn a_malformed_position_is_refused() {
        for token in ["called", "called[0]", "called[a][0]", "callee[0][0]", ""] {
            assert!(Position::parse(token).is_err(), "{token:?} should be refused");
        }
    }
}
