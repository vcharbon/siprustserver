//! Canonical `FeatureActivations` — the closed union of decision-engine
//! feature activations.
//!
//! Embedded in the `call` crate: `Call.features` carries it, so the data model
//! needs the type to round-trip. Until a cross-layer cycle forces a shared
//! crate, keeping it here respects ADR-0002 ("no premature shared types
//! crate").
//!
//! `platform` is mandatory; every feature arm is optional, and **absence means
//! "explicitly disabled," not "default enabled"** (the policy guard keys on
//! presence).

use serde::{Deserialize, Serialize};

/// Platform-mandatory keepalive activation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepaliveActivation {
    /// Seconds between OPTIONS pokes (or whatever keepalive mechanism is used).
    pub interval_sec: i64,
    /// Tear down the leg after this many unanswered keepalives.
    pub max_missed: i64,
}

/// Platform-mandatory cap + keepalive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformActivations {
    /// Overall call ceiling (seconds). Adapter supplies; platform caps it.
    pub max_duration_sec: i64,
    pub keepalive: KeepaliveActivation,
}

/// Optional REFER feature arm. Its **presence** is the decision layer's
/// directive that this call's transfers are processed LOCALLY — the platform
/// terminates the REFER (202 + `/call/refer` authorization + the transfer
/// slice) instead of passing it on. Absent, a REFER is an ordinary in-dialog
/// request relayed to the peer leg like INFO, and the RFC 3515 exchange
/// (202 / NOTIFY) rides end to end between the two peers.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferFeature {
    /// Caps REFER chain depth across attended transfers. `None` → unlimited.
    pub max_chain_depth: Option<i64>,
}

/// `relayFirst18xTo180` strategy — single-variant (mutually exclusive).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RelayFirst18xStrategy {
    DropSdp,
    KeepSdp,
    FakePrack,
    PromotePemTo200,
}

/// `relayFirst18xTo180` messages policy — WHICH 18x messages are relayed
/// toward the caller (each relayed one is downgraded per the machine's rules;
/// this only picks how many). Wire values of the Routing API `Relay18x.messages`
/// field (`ALL` / `FIRST` / `ONE_PER_VALUE`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Relay18xMessages {
    /// Relay every 18x (each downgraded).
    All,
    /// Relay only the first 18x; suppress the rest (the historical behavior).
    #[default]
    First,
    /// Relay one 18x per distinct upstream status value (first 180, first 183,
    /// …); suppress repeats of an already-relayed value.
    OnePerValue,
}

/// Optional `relayFirst18xTo180` feature arm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayFirst18xTo180Feature {
    pub strategy: RelayFirst18xStrategy,
    /// Which 18x messages are relayed (Routing API `Relay18x.messages`).
    /// Defaults to [`Relay18xMessages::First`] — today's behavior — and the
    /// serde default keeps old replicated bodies decoding unchanged.
    #[serde(default)]
    pub messages: Relay18xMessages,
}

/// One face's advertised capability set: the accepted methods (`Allow`, RFC
/// 3261 §20.5) and the understood option tags (`Supported`, §20.37), each as
/// its token list. This is the **replicated encoding** of the typed capability
/// value the SIP layer advertises — the data model holds tokens, not headers,
/// because it takes no `sip-message` dependency (ADR-0008).
///
/// The two halves are stated INDEPENDENTLY, so narrowing the methods never
/// forces a caller to restate (and freeze a copy of) the stack's option tags.
/// Per half: absent = advertise the stack's value for it; present and empty =
/// advertise the empty set, a value-less header line (§20.5 reads that as
/// "accepts no methods" — deliberately different from omitting the header); a
/// token that is not an RFC 3261 §25.1 `token` is dropped at the SIP boundary
/// and never reaches the wire.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertisedCapabilities {
    /// Accepted methods, e.g. `["INVITE", "ACK", "CANCEL", "BYE"]`.
    #[serde(default)]
    pub allow: Option<Vec<String>>,
    /// Understood option tags, e.g. `["timer"]`.
    #[serde(default)]
    pub supported: Option<Vec<String>>,
}

/// Optional per-face capability-advertisement arm. The two faces of a
/// back-to-back UA are independent so an asymmetric bridge can advertise a
/// narrow set toward one domain and the full set toward the other; an absent
/// face advertises the stack default, which is today's behaviour.
///
/// Scope of the declaration: the messages the stack MINTS — the INVITE it
/// originates, the INVITE 2xx it returns to the originator, and the re-INVITEs
/// it originates or relays. It does NOT rewrite the reliable-provisional
/// negotiation (`Require`/`Supported` on a relayed 1xx), which stays end-to-end
/// per RFC 3262. `toward_originated` covers EVERY originated leg —
/// the faces are two, not one per leg.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdvertiseCapabilitiesFeature {
    /// Advertised on messages the stack sends toward the originator (a-leg).
    #[serde(default)]
    pub toward_originator: Option<AdvertisedCapabilities>,
    /// Advertised on messages the stack sends toward an originated leg (b-leg).
    #[serde(default)]
    pub toward_originated: Option<AdvertisedCapabilities>,
}

/// Optional RFC 7315 §5.6 charging-correlation arm: the stack stamps a
/// `P-Charging-Vector` on every leg it ORIGINATES, so the records of the two
/// operators either side of it match on one identifier. A vector the originator
/// sent is relayed unchanged whether or not this arm is present — an identifier
/// re-minted mid-path breaks the correlation it exists for.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargingVectorFeature {
    /// The element the identifier is generated at (`icid-generated-at`).
    /// Absent — the stack's own SIP address.
    #[serde(default)]
    pub generated_at: Option<String>,
}

/// One entry in the optional `callLimiters` feature arm.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallLimiterFeatureEntry {
    pub id: String,
    pub limit: i64,
}

/// Closed feature-activation union: mandatory `platform` + optional arms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureActivations {
    pub platform: PlatformActivations,
    pub refer: Option<ReferFeature>,
    pub relay_first_18x_to_180: Option<RelayFirst18xTo180Feature>,
    pub no_answer_timeout_sec: Option<i64>,
    pub call_limiters: Option<Vec<CallLimiterFeatureEntry>>,
    /// Per-face `Allow`/`Supported` advertisement. Absent — and absent for one
    /// face — means "advertise the stack default there". `#[serde(default)]` so
    /// a body encoded before this arm decodes as no declaration.
    #[serde(default)]
    pub advertise_capabilities: Option<AdvertiseCapabilitiesFeature>,
    /// RFC 7315 §5.6 charging correlation on originated legs. Absent means the
    /// stack stamps none. `#[serde(default)]` so a body encoded before this arm
    /// decodes as no activation.
    #[serde(default)]
    pub charging_vector: Option<ChargingVectorFeature>,
    /// Option tags the B2BUA WITHHOLDS from every leg it originates: whatever
    /// `Supported` set would ride the originated INVITE is narrowed by these
    /// tags (an emptied set stays as the value-less line) and a relayed
    /// `Require` naming one is narrowed or dropped. Independent of the
    /// 18x-downgrade strategies — a declaration that never offers `100rel`
    /// leaves 18x relay untouched. The declaration is a call-lifetime LATCH:
    /// every applied route's list unions into the standing one
    /// ([`FeatureActivations::latch_withheld_option_tags`], run by
    /// `apply_route` and `SetFeatures`), so a failover route whose decision
    /// does not restate it cannot restore a withheld tag. `#[serde(default)]`
    /// so a body encoded before this arm decodes as no withhold.
    #[serde(default)]
    pub withhold_option_tags: Option<Vec<String>>,
}

impl FeatureActivations {
    /// The withhold latch: union `previous`'s withheld option tags into this
    /// route's declaration. A route can widen the withhold; none can restore a
    /// withheld tag — the property belongs to the call, not to the leg the
    /// declaring route dialled.
    pub fn latch_withheld_option_tags(&mut self, previous: Option<&FeatureActivations>) {
        let standing = previous.and_then(|f| f.withhold_option_tags.as_deref()).unwrap_or(&[]);
        if standing.is_empty() {
            return;
        }
        let mine = self.withhold_option_tags.get_or_insert_with(Vec::new);
        for tag in standing {
            if !mine.iter().any(|t| t.eq_ignore_ascii_case(tag)) {
                mine.push(tag.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn features(withheld: Option<&[&str]>) -> FeatureActivations {
        FeatureActivations {
            platform: PlatformActivations {
                max_duration_sec: 3_600,
                keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 2 },
            },
            refer: None,
            relay_first_18x_to_180: None,
            no_answer_timeout_sec: None,
            call_limiters: None,
            advertise_capabilities: None,
            charging_vector: None,
            withhold_option_tags: withheld.map(|w| w.iter().map(|t| t.to_string()).collect()),
        }
    }

    /// The withhold latch: a route that does not restate the withheld tags
    /// inherits the standing list; one that widens it keeps both, deduplicated
    /// case-insensitively (option tags are case-insensitive, RFC 3261 §7.3.1);
    /// and with nothing standing, nothing is invented.
    #[test]
    fn the_withhold_latch_unions_and_never_narrows() {
        let mut inherited = features(None);
        inherited.latch_withheld_option_tags(Some(&features(Some(&["100rel"]))));
        assert_eq!(
            inherited.withhold_option_tags.as_deref(),
            Some(["100rel".to_string()].as_slice())
        );

        let mut widened = features(Some(&["timer", "100REL"]));
        widened.latch_withheld_option_tags(Some(&features(Some(&["100rel"]))));
        assert_eq!(
            widened.withhold_option_tags.as_deref(),
            Some(["timer".to_string(), "100REL".into()].as_slice()),
        );

        let mut untouched = features(None);
        untouched.latch_withheld_option_tags(Some(&features(None)));
        assert_eq!(untouched.withhold_option_tags, None);
        untouched.latch_withheld_option_tags(None);
        assert_eq!(untouched.withhold_option_tags, None);
    }
}
