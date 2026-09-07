//! The `msg` spec (`PCAP2TEST_PIVOT_V3.md` §8): what a step sends or expects,
//! under the three-tier model.
//!
//! - **Tier 1, stack-owned**, never stored: Via and branch, Call-ID, From/To
//!   tags, CSeq numbering, Max-Forwards, Content-Length, Contact host:port,
//!   route sets. The list itself is exported by [`crate::tiers`].
//! - **Tier 2, role-mapped**, stored symbolically as a [`Ref`].
//! - **Tier 3, frozen**, stored verbatim in wire order.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::body::Body;
use crate::scoping::CheckClass;

/// The message a step sends or expects.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MsgSpec {
    /// Request discriminator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    /// Response discriminator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// The response's reason phrase, as the capture carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// On a response: the CSeq method of the transaction it answers. A leg
    /// carries the INVITE alongside every automatic, so without it an
    /// automatic's 2xx satisfies the expectation meant for the call's answer.
    #[serde(rename = "cseq-method", default, skip_serializing_if = "Option::is_none")]
    pub cseq_method: Option<String>,
    /// The captured CSeq NUMBER, on `auto` steps only: the LABEL of the
    /// transaction that obliged the message. An identity token for pairing,
    /// lint and confrontation, and the marker that scopes an auto ACK's DRAWN
    /// `retransmits` to one transaction — NEVER replayed and never resolved
    /// against: the interpreter's own stack numbers its CSeqs, and an auto ACK
    /// finds the final it acknowledges in leg state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cseq: Option<u32>,
    /// Tier-2 refs, on dialog-opening INVITE sends. An in-dialog R-URI is the
    /// tier-1 learned remote target and is regenerated instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ruri: Option<Ref>,
    /// Tier-2 ref for the From address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Ref>,
    /// Tier-2 ref for the To address.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<Ref>,
    /// Tier-3 frozen headers, in wire order with casing and duplicates
    /// preserved. Stated on an `expect` regardless of `check`: under `assert`
    /// they are matched, under `record` they are the recorded value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub headers: Vec<Header>,
    /// Existence checks on an expect.
    #[serde(rename = "headers-present", default, skip_serializing_if = "Vec::is_empty")]
    pub headers_present: Vec<String>,
    /// The body, by resource reference, multipart structure or asserted shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<Body>,
}

/// One frozen header, exactly as the capture carried it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Header {
    /// Header name, in the capture's own spelling and casing.
    pub name: String,
    /// Header value, verbatim — or, in an authored document, composed from
    /// `${…}` accessors.
    pub value: String,
    /// Which vocabulary this header is (§9.1), where it is the ORIGIN
    /// platform's rather than SIP's. Under `check: assert` a classified header
    /// is matched on the lane the document came from and merely recorded
    /// elsewhere; an unclassified header is matched on every lane.
    #[serde(rename = "class", default, skip_serializing_if = "Option::is_none")]
    pub class: Option<CheckClass>,
}

/// A tier-2 reference: either the numbering plan recognized the value and the
/// document stores its ROLE, or it did not and the document freezes the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum Ref {
    /// Role-mapped: the lane substitutes the number it allocated for this
    /// position.
    Positional(PositionalRef),
    /// Frozen: the plan did not recognize the value, so it replays verbatim.
    Frozen(FrozenRef),
}

/// A tier-2 reference the numbering plan resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PositionalRef {
    /// Position token in the tier-2 reference namespace, resolved against the
    /// call chains: `caller` or `called[<branch>][<position>]`, call-qualified
    /// (`c2.called[0][1]`) once the document declares more than one call.
    pub pos: String,
    /// Open dial-form token: which form of the number this field carried.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form: Option<String>,
}

/// A tier-2 reference the numbering plan did not resolve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FrozenRef {
    /// The captured value, replayed verbatim.
    pub frozen: String,
    /// Open plan token qualifying the frozen value where the plan classified
    /// the ADDRESS without resolving a number (an anonymous caller carries no
    /// number to map, but the class still drives emission).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Ref {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn a_ref_is_either_role_mapped_or_frozen() {
        assert!(matches!(parse(r#"{"pos":"caller","form":"private"}"#), Ref::Positional(_)));
        assert!(matches!(parse(r#"{"frozen":"0099999900011"}"#), Ref::Frozen(_)));
        assert!(matches!(
            parse(r#"{"frozen":"anonymous@anonymous.invalid","kind":"anonymous"}"#),
            Ref::Frozen(_)
        ));
    }

    #[test]
    fn a_ref_that_is_neither_shape_is_refused() {
        assert!(serde_json::from_str::<Ref>(r#"{"form":"e164"}"#).is_err());
        assert!(serde_json::from_str::<Ref>(r#"{"pos":"caller","frozen":"x"}"#).is_err());
        assert!(serde_json::from_str::<Ref>(r#"{"pos":"caller","posn":1}"#).is_err());
    }

    #[test]
    fn a_frozen_header_may_name_the_platform_whose_vocabulary_it_is() {
        let classified: Header = serde_json::from_str(
            r#"{"name":"P-Charging-Vector","value":"icid-value=x","class":"origin-platform-header"}"#,
        )
        .unwrap();
        assert_eq!(classified.class, Some(CheckClass::OriginPlatformHeader));
        let plain: Header = serde_json::from_str(r#"{"name":"Allow","value":"INVITE"}"#).unwrap();
        assert_eq!(plain.class, None);
        assert!(!serde_json::to_string(&plain).unwrap().contains("class"));
    }

    #[test]
    fn an_unknown_msg_field_is_refused_rather_than_ignored() {
        assert!(serde_json::from_str::<MsgSpec>(r#"{"method":"INVITE","observed-headers":[]}"#).is_err());
    }
}
