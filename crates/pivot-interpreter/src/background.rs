//! **Background traffic** (`PCAP2TEST_PIVOT_V3.md` §5.1): the messages an actor
//! answers outside the flow.
//!
//! A message matching a policy NEVER touches the flow cursor. It is answered per
//! `respond`, recorded in the run bundle, and never satisfies an `expect`. No
//! flow step is ever written for one — this is where the v0.1 interpreter's
//! keepalive elision lives now, as document data rather than interpreter logic.
//!
//! Outside the flow is the whole scope: an arrival a frontier `expect` on that
//! leg is OPEN on belongs to that step, and `exec` offers the policy only what
//! no such step claims.
//!
//! `count` is checked at SETTLE, never mid-flow: "the endpoint was polled while
//! the call ran" is a fact about a period, not about a position in a sequence.
//! `exactly: 0` is how a document states an absence, and a policy stating no
//! bound answers the traffic and asserts nothing about it.

use std::collections::BTreeMap;

use pivot_schema::bundle::Failure;
use pivot_schema::placement::CountBound;
use sip_message::Method;


/// One policy, bound to the actor that owns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    pub actor: String,
    pub method: String,
    pub status: u16,
    pub count: Option<CountBound>,
}

/// Every background policy of a run, and how much traffic each answered.
#[derive(Debug, Clone, Default)]
pub struct Background {
    policies: Vec<Policy>,
    /// Policy index → how many messages it answered.
    answered: BTreeMap<usize, u32>,
}

impl Background {
    /// Build the table from the actors' declared policies.
    pub fn new(policies: Vec<Policy>) -> Self {
        Background { policies, answered: BTreeMap::new() }
    }

    /// The policy of `actor` that answers `method`, if any. Matching is by
    /// method identity, so a policy written `options` answers an `OPTIONS`.
    pub fn policy_for(&self, actor: &str, method: &str) -> Option<(usize, &Policy)> {
        let wanted = Method::from_wire(method);
        self.policies
            .iter()
            .enumerate()
            .find(|(_, p)| p.actor == actor && Method::from_wire(&p.method) == wanted)
    }

    /// Note that policy `index` answered one message.
    pub fn note_answered(&mut self, index: usize) {
        *self.answered.entry(index).or_default() += 1;
    }

    /// How many messages policy `index` answered.
    pub fn count(&self, index: usize) -> u32 {
        self.answered.get(&index).copied().unwrap_or(0)
    }

    /// The settle-time counter check: every stated bound, over every policy.
    pub fn settle_failures(&self) -> Vec<Failure> {
        let mut out = Vec::new();
        for (index, policy) in self.policies.iter().enumerate() {
            let Some(bound) = policy.count else { continue };
            let observed = self.count(index);
            if !satisfies(&bound, observed) {
                out.push(Failure::BackgroundCount {
                    actor: policy.actor.clone(),
                    method: policy.method.clone(),
                    bound: describe(&bound),
                    observed,
                });
            }
        }
        out
    }
}

/// Whether `observed` satisfies every bound the document stated. A bound that
/// says nothing (`{}`) satisfies nothing: it is a document defect the plan
/// refuses, not a pass.
pub fn satisfies(bound: &CountBound, observed: u32) -> bool {
    if !bound.is_satisfiable() {
        return false;
    }
    if let Some(exactly) = bound.exactly {
        return observed == exactly;
    }
    bound.at_least.is_none_or(|low| observed >= low)
        && bound.at_most.is_none_or(|high| observed <= high)
}

/// A bound, as a failure reads it.
pub fn describe(bound: &CountBound) -> String {
    match (bound.exactly, bound.at_least, bound.at_most) {
        (Some(n), _, _) => format!("exactly {n}"),
        (None, Some(low), Some(high)) => format!("at least {low} and at most {high}"),
        (None, Some(low), None) => format!("at least {low}"),
        (None, None, Some(high)) => format!("at most {high}"),
        (None, None, None) => "no bound".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bound(text: &str) -> CountBound {
        serde_json::from_str(text).unwrap()
    }

    fn policy(actor: &str, method: &str, count: Option<CountBound>) -> Policy {
        Policy { actor: actor.into(), method: method.into(), status: 200, count }
    }

    #[test]
    fn every_stated_bound_is_checked_and_an_unstated_one_asserts_nothing() {
        assert!(satisfies(&bound(r#"{"at_least":1}"#), 1));
        assert!(!satisfies(&bound(r#"{"at_least":1}"#), 0));
        assert!(satisfies(&bound(r#"{"exactly":2}"#), 2));
        assert!(!satisfies(&bound(r#"{"exactly":2}"#), 3));
        assert!(satisfies(&bound(r#"{"exactly":0}"#), 0));
        assert!(!satisfies(&bound(r#"{"exactly":0}"#), 1));
        assert!(satisfies(&bound(r#"{"at_least":1,"at_most":3}"#), 3));
        assert!(!satisfies(&bound(r#"{"at_least":1,"at_most":3}"#), 4));
        // A bound that says nothing, or contradicts itself, is never satisfied.
        assert!(!satisfies(&bound("{}"), 0));
        assert!(!satisfies(&bound(r#"{"at_least":3,"at_most":1}"#), 2));
    }

    #[test]
    fn a_policy_answers_by_method_identity_and_only_for_its_own_actor() {
        let table = Background::new(vec![
            policy("uas1", "options", None),
            policy("uac1", "OPTIONS", None),
        ]);
        assert_eq!(table.policy_for("uas1", "OPTIONS").unwrap().0, 0);
        assert_eq!(table.policy_for("uac1", "OPTIONS").unwrap().0, 1);
        assert!(table.policy_for("uas1", "INFO").is_none());
        assert!(table.policy_for("uas2", "OPTIONS").is_none());
    }

    #[test]
    fn counters_are_checked_at_settle_and_name_the_bound_they_missed() {
        let mut table = Background::new(vec![
            policy("uas1", "OPTIONS", Some(bound(r#"{"exactly":2}"#))),
            policy("uac1", "NOTIFY", Some(bound(r#"{"exactly":0}"#))),
        ]);
        table.note_answered(0);
        assert_eq!(table.count(0), 1);
        let failures = table.settle_failures();
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(
            matches!(&failures[0], Failure::BackgroundCount { bound, observed: 1, .. } if bound == "exactly 2")
        );
        table.note_answered(0);
        assert!(table.settle_failures().is_empty());
        table.note_answered(1);
        assert_eq!(table.settle_failures().len(), 1, "an `exactly: 0` absence that fired fails");
    }
}
