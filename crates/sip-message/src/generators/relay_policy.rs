//! A deployment's static relay policy: the headers a relayed message leaves
//! behind, named by header, by the message being relayed and by the way it
//! travels. Transparency (RFC 3261 §16.6) stays the default; an entry is a
//! deployment's statement that a header stops at this element on that message.

use crate::header::HeaderName;
use crate::method::Method;

/// The way a relayed message travels across the back-to-back UA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayDirection {
    /// Toward the party that sent the dialog-creating INVITE.
    TowardCaller,
    /// Toward a party the back-to-back UA called.
    TowardCallee,
}

/// The message a relay mints, as the policy reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RelayedMessage<'m> {
    /// A request of this method.
    Request(&'m Method),
    /// A response of this status to a request of this method.
    Response { status: u16, method: &'m Method },
}

/// The message a relay mints and the way it travels: what a policy entry is
/// matched against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RelaySituation<'m> {
    pub message: RelayedMessage<'m>,
    pub toward: RelayDirection,
}

impl<'m> RelaySituation<'m> {
    /// A request of `method` relayed `toward`.
    pub const fn request(method: &'m Method, toward: RelayDirection) -> Self {
        Self { message: RelayedMessage::Request(method), toward }
    }

    /// A response of `status` to a `method` request, relayed `toward`.
    pub const fn response(status: u16, method: &'m Method, toward: RelayDirection) -> Self {
        Self { message: RelayedMessage::Response { status, method }, toward }
    }
}

/// The messages one entry names: every request of a method, or every response
/// of a status class (`2` for 2xx) to a request of a method.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum MessageClass {
    Request(Method),
    Response { class: u16, method: Method },
}

impl MessageClass {
    fn names(&self, message: RelayedMessage<'_>) -> bool {
        match (self, message) {
            (MessageClass::Request(m), RelayedMessage::Request(sent)) => m == sent,
            (
                MessageClass::Response { class, method },
                RelayedMessage::Response { status, method: sent },
            ) => status / 100 == *class && method == sent,
            _ => false,
        }
    }
}

/// One entry: `header` stays behind on every message of `class` relayed in
/// `toward`'s direction, or in either direction where `toward` is `None`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RelayDrop {
    pub header: HeaderName,
    pub class: MessageClass,
    pub toward: Option<RelayDirection>,
}

impl RelayDrop {
    fn drops(&self, name: &str, situation: RelaySituation<'_>) -> bool {
        self.header.matches(name)
            && self.class.names(situation.message)
            && self.toward.is_none_or(|toward| toward == situation.toward)
    }
}

/// The deployment's relay policy. Empty by default: every relayable header
/// rides. An entry only ever removes a header the relay would otherwise carry;
/// it states nothing, so a header a decision states on the message still rides.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct RelayPolicy {
    drops: Vec<RelayDrop>,
}

impl RelayPolicy {
    /// The policy that drops nothing.
    pub const fn transparent() -> Self {
        Self { drops: Vec::new() }
    }

    /// This policy plus one entry.
    pub fn dropping(
        mut self,
        header: &str,
        class: MessageClass,
        toward: Option<RelayDirection>,
    ) -> Self {
        self.drops.push(RelayDrop { header: HeaderName::from(header), class, toward });
        self
    }

    /// The entries, in the order they were stated.
    pub fn entries(&self) -> &[RelayDrop] {
        &self.drops
    }

    /// True iff a header named `name` stays behind on the message `situation`
    /// describes.
    pub fn drops(&self, name: &str, situation: RelaySituation<'_>) -> bool {
        self.drops.iter().any(|entry| entry.drops(name, situation))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use RelayDirection::{TowardCallee, TowardCaller};

    fn identity_on_answers_and_teardown() -> RelayPolicy {
        RelayPolicy::transparent()
            .dropping(
                "P-Asserted-Identity",
                MessageClass::Response { class: 2, method: Method::Invite },
                None,
            )
            .dropping("P-Asserted-Identity", MessageClass::Request(Method::Bye), Some(TowardCallee))
    }

    #[test]
    fn an_entry_names_a_response_class_to_one_method() {
        let policy = identity_on_answers_and_teardown();
        let ok = RelaySituation::response(200, &Method::Invite, TowardCaller);
        let ringing = RelaySituation::response(180, &Method::Invite, TowardCaller);
        let update_ok = RelaySituation::response(200, &Method::Update, TowardCaller);
        assert!(policy.drops("P-Asserted-Identity", ok));
        assert!(policy.drops("p-asserted-identity", ok), "names match case-insensitively");
        assert!(!policy.drops("P-Asserted-Identity", ringing), "another status class");
        assert!(!policy.drops("P-Asserted-Identity", update_ok), "another method");
        assert!(!policy.drops("P-Preferred-Identity", ok), "another header");
    }

    #[test]
    fn an_entry_with_a_direction_holds_only_that_way() {
        let policy = identity_on_answers_and_teardown();
        let to_callee = RelaySituation::request(&Method::Bye, TowardCallee);
        let to_caller = RelaySituation::request(&Method::Bye, TowardCaller);
        assert!(policy.drops("P-Asserted-Identity", to_callee));
        assert!(!policy.drops("P-Asserted-Identity", to_caller));
        let answer_to_callee = RelaySituation::response(200, &Method::Invite, TowardCallee);
        assert!(policy.drops("P-Asserted-Identity", answer_to_callee), "no direction: both ways");
    }

    #[test]
    fn a_request_entry_never_names_a_response() {
        let policy = identity_on_answers_and_teardown();
        let bye_ok = RelaySituation::response(200, &Method::Bye, TowardCallee);
        assert!(!policy.drops("P-Asserted-Identity", bye_ok));
    }

    #[test]
    fn an_entry_reads_a_compact_form_as_its_header() {
        let policy = RelayPolicy::transparent().dropping(
            "Supported",
            MessageClass::Request(Method::Invite),
            None,
        );
        let invite = RelaySituation::request(&Method::Invite, TowardCallee);
        assert!(policy.drops("k", invite), "k = Supported");
    }

    #[test]
    fn the_transparent_policy_drops_nothing() {
        let invite_ok = RelaySituation::response(200, &Method::Invite, TowardCaller);
        assert!(!RelayPolicy::transparent().drops("P-Asserted-Identity", invite_ok));
        assert!(RelayPolicy::default().entries().is_empty());
    }
}
