//! `endpoints`, `actors` and `legs` (`PCAP2TEST_PIVOT_V3.md` §5): where the
//! replay binds, what it simulates there, and the symbolic dialogs it plays.
//!
//! An **endpoint** is one mux socket. An **actor** is one simulated network
//! element on an endpoint. A **leg** is one symbolic dialog. Call-ID, tags,
//! CSeq base and route set are runner state, referenced only through a leg id.
//!
//! An actor also states its **background policy**: the traffic it answers
//! without the flow noticing. That policy is DOCUMENT DATA on purpose — the
//! v0.1 interpreter elided keepalives with logic of its own, and callflow
//! knowledge inside an interpreter is the failure mode this format exists to
//! prevent.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// One socket the lane must bind.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Endpoint {
    /// Unique within the document; what an actor names.
    pub id: String,
    /// The captured socket. A lane binding by address uses it; nothing else in
    /// the document carries one.
    pub observed: String,
    /// What the endpoint is relative to the system under test.
    pub side: Side,
    /// How the lane must bind it.
    pub binding: Binding,
}

/// What an endpoint IS relative to the system under test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    /// A simulated peer: the replay binds it and plays an actor there.
    Peer,
    /// The system under test's own socket: the replay sends to it and never
    /// binds it.
    Sut,
}

/// How the lane must bind an endpoint. Stated PER ENDPOINT: a case may mix a
/// loopback vantage on one attempt with a dedicated one on the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Binding {
    /// The endpoint hosts one role, on its own socket.
    Dedicated,
    /// The endpoint hosts both a UAC and a UAS on one socket.
    Loopback,
}

/// One simulated network element.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    /// Unique within the document; what a leg names.
    pub id: String,
    /// The SIP role the actor plays.
    pub kind: ActorKind,
    /// Endpoint id.
    pub endpoint: String,
    /// Name of the actor's own identity in the `identities` registry — the
    /// CALLER's, in practice: a callee's identity is named by its attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// How a UAS claims its inbound INVITE.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim: Option<Claim>,
    /// Traffic this actor answers OUTSIDE the flow: matching messages never
    /// move the flow cursor.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background: Vec<BackgroundPolicy>,
}

/// One class of background traffic an actor answers, and what the run must
/// have seen of it by settle.
///
/// A matching message is answered per `respond` and recorded; it never
/// satisfies an `expect`, and no flow step is ever written for one. An
/// assertion about background traffic is a COUNTER checked at settle, because
/// "the endpoint was polled while the call ran" is a fact about a period, not
/// about a position in a sequence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundPolicy {
    /// What the policy answers.
    pub r#match: BackgroundMatch,
    /// How it answers.
    pub respond: BackgroundResponse,
    /// How many such messages the run must have seen, checked at settle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<CountBound>,
}

/// What a background policy matches. Method-only today: a keepalive is
/// identified by its method, and a policy that had to inspect a header would
/// be a flow step wearing a disguise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundMatch {
    /// The method this policy answers. Matching messages never move the flow
    /// cursor and never satisfy an `expect`.
    pub method: String,
}

/// How a background policy answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BackgroundResponse {
    /// The status the actor answers with. Mandatory: a policy is never silent.
    pub status: u16,
}

/// A settle-time count assertion. Every bound stated is checked; stating none
/// means the traffic is answered and not asserted about.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CountBound {
    /// Lower bound on the messages this policy answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_least: Option<u32>,
    /// Upper bound. `at_most: 0` and `exactly: 0` are how a document states an
    /// absence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_most: Option<u32>,
    /// Exact count. Exclusive with the two bounds above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exactly: Option<u32>,
}

impl CountBound {
    /// Whether the bound says anything, and says it without contradicting
    /// itself.
    pub fn is_satisfiable(&self) -> bool {
        if self.exactly.is_some() && (self.at_least.is_some() || self.at_most.is_some()) {
            return false;
        }
        match (self.at_least, self.at_most) {
            (Some(low), Some(high)) => low <= high,
            _ => self.exactly.is_some() || self.at_least.is_some() || self.at_most.is_some(),
        }
    }
}

/// The SIP role an actor plays.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ActorKind {
    /// Places calls: originates the dialogs its legs open.
    Uac,
    /// Takes calls: answers the INVITEs it claims.
    Uas,
    /// Media resource: answers, plays, and takes INFO rather than routing.
    Mrf,
}

/// How a UAS decides that an inbound INVITE is the one it is playing. Carries
/// `by` and nothing else: which attempt an actor plays is already stated by
/// `calls[].attempts[].leg`, and restating it would give the chain two
/// encodings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    /// What tells this actor's inbound INVITE from another's.
    pub by: ClaimBy,
}

/// The claim discriminator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum ClaimBy {
    /// The R-URI carries the number allocated to this actor's attempt.
    RuriPos,
    /// Nothing distinguishes the INVITEs; the Nth arrival is the Nth claim.
    ArrivalOrder,
}

/// One symbolic dialog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Leg {
    /// Unique within the document; what every flow step and leg accessor names.
    pub id: String,
    /// Actor id.
    pub actor: String,
    /// Which side of the dialog the actor is on.
    pub dir: Direction,
    /// Per-leg RTP source. Omitted where the leg carries no media.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<Media>,
}

/// Which side of the dialog the actor is on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// The actor originates the dialog.
    Out,
    /// The actor receives it.
    In,
}

/// Media the leg carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Media {
    /// Open token naming how the lane sources RTP for this leg.
    pub rtp: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(text: &str) -> Actor {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn an_actor_without_a_background_policy_writes_none() {
        let a = actor(r#"{"id":"uas1","kind":"uas","endpoint":"ep0"}"#);
        assert!(a.background.is_empty());
        assert!(!serde_json::to_string(&a).unwrap().contains("background"));
    }

    #[test]
    fn a_background_policy_answers_a_method_and_may_bound_the_count() {
        let a = actor(
            r#"{"id":"uas1","kind":"uas","endpoint":"ep0","background":[
                 {"match":{"method":"OPTIONS"},"respond":{"status":200},"count":{"at_least":1}}]}"#,
        );
        assert_eq!(a.background[0].r#match.method, "OPTIONS");
        assert_eq!(a.background[0].respond.status, 200);
        assert!(a.background[0].count.unwrap().is_satisfiable());
    }

    #[test]
    fn a_count_bound_that_says_nothing_or_contradicts_itself_is_unsatisfiable() {
        let bound = |text: &str| serde_json::from_str::<CountBound>(text).unwrap();
        assert!(bound(r#"{"exactly":0}"#).is_satisfiable());
        assert!(bound(r#"{"at_least":1,"at_most":3}"#).is_satisfiable());
        assert!(!bound("{}").is_satisfiable());
        assert!(!bound(r#"{"at_least":3,"at_most":1}"#).is_satisfiable());
        assert!(!bound(r#"{"exactly":1,"at_least":1}"#).is_satisfiable());
    }
}
