//! What CANCELS a scripted party's RFC finding (`PCAP2TEST_PIVOT_V3.md` §11.1):
//! the document's own statement of the same violation, against the same party,
//! on the same transaction. One concern: reading those statements off the
//! document and matching a finding's offending datagram to one through the
//! run's recording — which is also what names the actor a finding is charged
//! to, since several actors may share one endpoint.
//!
//! A statement is an `rfc_violations` entry naming an actor — on a captured
//! case, the census's reading of the captured trace — or a `suppress-auto`
//! deviation withholding the ACK or PRACK an omission rule misses, its author's
//! statement of the behaviour. Nothing else cancels: a scripted party's finding
//! the document does not state is a source fault (`crate::audit`), unless the
//! document says it never read its captured trace's violations
//! ([`UNCOVERED_FLAG`]), which leaves the finding unjudged.
//!
//! **Relayed.** The system under test's own finding is cancelled by one thing
//! only: a `sut` entry that `relays` a scripted party's entry of the same rule,
//! the two together stating that the party committed the violation and the
//! captured platform carried it on. The finding is then attributed to that
//! party (`Relayed`); either entry alone cancels nothing of the SUT's. What it
//! leaves gating, a post-run bless may still read back to a peer, where the run
//! shows ours carried the peer's bytes unchanged.
//!
//! **Same transaction.** A statement lands on a step; the transaction is the one
//! that step's message rides, named in the run by the datagrams the interpreter
//! attributed to that step or to the request that opened its transaction. The
//! finding's offending datagram names its own by Call-ID, top-Via branch and
//! CSeq (RFC 3261 §17.1.3), and the two must meet on the statement's leg.

use std::collections::BTreeMap;

use pivot_schema::bundle::RecordedMessage;
use pivot_schema::flow::{Op, Step};
use pivot_schema::violation::SUT_EMITTER;
use pivot_schema::PivotV3;
use sip_message::sniff;

/// The rules a withheld ACK breaks: the 2xx's (§13.2.2.4) and the reject's
/// (§17.1.1.3).
const WITHHELD_ACK: [&str; 2] = ["no-ack-to-dialog-creating-2xx", "unacked-invite-non-2xx-final"];
/// The rule a withheld PRACK breaks (RFC 3262 §4).
const WITHHELD_PRACK: [&str; 1] = ["unacked-reliable-provisional"];
/// The flag a captured document carries where the violation census never
/// covered its capture, so no entry states what its scripted parties broke.
pub const UNCOVERED_FLAG: &str = "census-uncovered";
/// What an unjudged finding says of a document carrying [`UNCOVERED_FLAG`].
const UNREAD: &str = "census-uncovered: the census never read this capture's violations, \
                      so the document states none of its scripted parties'";

/// One statement of a scripted party's violation, located in the document.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Statement {
    /// The rule tokens it states.
    rules: Vec<String>,
    /// The actor it charges.
    actor: String,
    /// The leg its step rides.
    leg: String,
    /// The steps whose attributed datagrams name its transaction.
    steps: Vec<String>,
    /// How a finding it cancels names it.
    label: String,
    /// On a relayed statement, the party that originated the violation.
    caused_by: Option<String>,
}

/// Every statement a document makes, against the run's recording.
#[derive(Debug, Default)]
pub struct Statements {
    statements: Vec<Statement>,
    legs: BTreeMap<String, Vec<RecordedMessage>>,
    /// Leg id → the actor that plays it, and that actor's endpoint id.
    players: BTreeMap<String, (String, String)>,
    /// The document's own word that it never read its captured trace's
    /// violations ([`UNCOVERED_FLAG`]), where it carries one.
    unread: Option<String>,
}

impl Statements {
    /// The statements `document` makes, matched against `legs`, the run's
    /// per-leg recording.
    pub fn of(document: &PivotV3, legs: BTreeMap<String, Vec<RecordedMessage>>) -> Statements {
        let steps = document.steps();
        let players: BTreeMap<String, (String, String)> = document
            .legs
            .iter()
            .filter_map(|l| {
                let actor = document.actors.iter().find(|a| a.id == l.actor)?;
                Some((l.id.clone(), (actor.id.clone(), actor.endpoint.clone())))
            })
            .collect();
        let mut statements = Vec::new();
        for violation in document.rfc_violations.iter().filter(|v| !v.sut_emitted()) {
            let Some(at) = steps.iter().position(|s| s.id == violation.step) else { continue };
            statements.push(Statement {
                rules: vec![violation.rule.to_string()],
                actor: violation.emitter.clone(),
                leg: steps[at].leg.clone(),
                steps: transaction_steps(&steps, at),
                label: format!(
                    "rfc_violations: {} at {} by {}",
                    violation.rule, violation.step, violation.emitter
                ),
                caused_by: None,
            });
        }
        for violation in document.rfc_violations.iter().filter(|v| v.sut_emitted()) {
            let Some(origin) = &violation.relays else { continue };
            let Some(party) = document
                .rfc_violations
                .iter()
                .find(|v| !v.sut_emitted() && v.rule == violation.rule && &v.step == origin)
            else {
                continue;
            };
            let Some(at) = steps.iter().position(|s| s.id == violation.step) else { continue };
            statements.push(Statement {
                rules: vec![violation.rule.to_string()],
                actor: SUT_EMITTER.to_string(),
                leg: steps[at].leg.clone(),
                steps: transaction_steps(&steps, at),
                label: format!(
                    "relayed: {} originated by {} at {}; the reference relays it too",
                    violation.rule, party.emitter, party.step
                ),
                caused_by: Some(party.emitter.clone()),
            });
        }
        for deviation in document.deviations.iter().filter(|d| d.kind == "suppress-auto") {
            let Some(withheld) = &deviation.step else { continue };
            let Some(at) = steps.iter().position(|s| &s.id == withheld) else { continue };
            let step = steps[at];
            let (rules, answered): (&[&str], fn(u16) -> bool) =
                match step.msg.method.as_deref().map(str::to_ascii_uppercase).as_deref() {
                    Some("ACK") => (&WITHHELD_ACK, |status| status >= 200),
                    Some("PRACK") => (&WITHHELD_PRACK, |status| (101..200).contains(&status)),
                    _ => continue,
                };
            let Some(taken) = taken_before(&steps, at, answered) else { continue };
            let Some((actor, _)) = players.get(&step.leg) else { continue };
            statements.push(Statement {
                rules: rules.iter().map(|r| r.to_string()).collect(),
                actor: actor.clone(),
                leg: step.leg.clone(),
                steps: transaction_steps(&steps, taken),
                label: format!("deviation {} (suppress-auto) withholds {withheld}", deviation.id),
                caused_by: None,
            });
        }
        let unread = document
            .case
            .annotations
            .iter()
            .flat_map(|a| &a.flags)
            .any(|flag| flag.kind == UNCOVERED_FLAG)
            .then(|| UNREAD.to_string());
        Statements { statements, legs, players, unread }
    }

    /// The actor a finding charged to `endpoint` names: the one playing the leg
    /// whose recording carries the offending datagram's Call-ID, where that
    /// actor binds `endpoint` — `None` where no leg of the run carries it.
    pub fn actor_named(&self, endpoint: &str, offending: &[u8]) -> Option<String> {
        let call_id = sniff::call_id(offending)?;
        self.legs
            .iter()
            .filter(|(_, recorded)| {
                recorded.iter().any(|m| sniff::call_id(m.wire()).as_deref() == Some(&call_id))
            })
            .find_map(|(leg, _)| {
                self.players
                    .get(leg)
                    .filter(|(_, bound)| bound == endpoint)
                    .map(|(actor, _)| actor.clone())
            })
    }

    /// The statement that cancels the finding of `rule` against `actor`, whose
    /// offending datagram is `offending` — `None` where the document states no
    /// such violation on that transaction.
    pub fn cancelling(&self, rule: &str, actor: &str, offending: &[u8]) -> Option<String> {
        self.stated_on(rule, offending, |s| s.caused_by.is_none() && s.actor == actor)
            .map(|s| s.label.clone())
    }

    /// The document's attribution of the system under test's finding of
    /// `rule`, whose offending datagram is `offending`, to the party that
    /// originated it: the `sut` entry relaying the rule on that transaction,
    /// beside that party's own entry — `None` where the document states less.
    pub fn relayed(&self, rule: &str, offending: &[u8]) -> Option<Relayed> {
        let s = self.stated_on(rule, offending, |s| s.caused_by.is_some())?;
        Some(Relayed { label: s.label.clone(), caused_by: s.caused_by.clone()? })
    }

    /// Why the document states none of its scripted parties' violations, where
    /// it says it never read its captured trace for them.
    pub fn unread(&self) -> Option<String> {
        self.unread.clone()
    }

    /// The run's recording, by leg, the statements are read against.
    pub fn legs(&self) -> &BTreeMap<String, Vec<RecordedMessage>> {
        &self.legs
    }

    /// The first statement `admits` of `rule` on the transaction `offending`
    /// rides, met on the statement's leg through the steps it names.
    fn stated_on(
        &self,
        rule: &str,
        offending: &[u8],
        admits: impl Fn(&Statement) -> bool,
    ) -> Option<&Statement> {
        let key = transaction(offending)?;
        self.statements.iter().filter(|s| s.rules.iter().any(|r| r == rule) && admits(s)).find(
            |s| {
                self.legs.get(&s.leg).is_some_and(|recorded| {
                    recorded.iter().any(|m| {
                        m.step.as_ref().is_some_and(|step| s.steps.contains(step))
                            && transaction(m.wire()).as_ref() == Some(&key)
                    })
                })
            },
        )
    }
}

/// A system-under-test finding the document attributes to another party.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Relayed {
    /// How the cancelled finding names the two statements.
    pub label: String,
    /// The scripted party that originated the violation.
    pub caused_by: String,
}

/// A datagram's transaction as RFC 3261 §17.1.3 keys it: Call-ID, top-Via
/// branch, CSeq number and method.
fn transaction(raw: &[u8]) -> Option<(String, String, u32, String)> {
    Some((
        sniff::call_id(raw)?,
        sniff::via_branch(raw)?,
        sniff::cseq_number(raw)?,
        sniff::cseq_method_token(raw)?,
    ))
}

/// The steps that name the transaction `steps[at]` rides: the step itself and,
/// for a response, the request on its leg that opened the transaction — the
/// last request of the response's CSeq method the other side of the leg put
/// before it.
fn transaction_steps(steps: &[&Step], at: usize) -> Vec<String> {
    let step = steps[at];
    let mut named = vec![step.id.clone()];
    if step.msg.status.is_none() {
        return named;
    }
    let Some(method) = &step.msg.cseq_method else { return named };
    let opener = steps[..at].iter().rev().find(|s| {
        s.leg == step.leg
            && s.op != step.op
            && s.msg.method.as_deref().is_some_and(|m| m.eq_ignore_ascii_case(method))
    });
    named.extend(opener.map(|s| s.id.clone()));
    named
}

/// The response the withheld message at `steps[at]` answers: the last one its
/// leg TOOK before it whose status `answered` admits, to an INVITE.
fn taken_before(steps: &[&Step], at: usize, answered: fn(u16) -> bool) -> Option<usize> {
    let leg = &steps[at].leg;
    (0..at).rev().find(|&i| {
        let s = steps[i];
        &s.leg == leg
            && s.op == Op::Expect
            && s.msg.status.is_some_and(answered)
            && s.msg.cseq_method.as_deref().is_none_or(|m| m.eq_ignore_ascii_case("INVITE"))
    })
}

#[cfg(test)]
#[path = "cancellation_tests.rs"]
mod tests;
