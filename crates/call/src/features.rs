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

use crate::header_update::SipHeaderUpdates;

/// Platform-mandatory keepalive activation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeepaliveActivation {
    /// Seconds between OPTIONS pokes (or whatever keepalive mechanism is used).
    pub interval_sec: i64,
    /// Tear down the leg after this many unanswered keepalives.
    pub max_missed: i64,
}

/// Where the overall call ceiling is anchored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MaxDurationAnchor {
    /// The cap bounds the whole call from its creation: armed at route time, it
    /// reaps a setup that never completes as well as the established call.
    #[default]
    Creation,
    /// The cap bounds the established call: it runs from the answer the caller
    /// receives. The setup is bounded by its own deadline (`SetupTimeout`); where
    /// none is configured the cap is armed at creation as under `Creation`, so
    /// no call is ever without a reaper.
    Answer,
}

/// Platform-mandatory cap + keepalive.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformActivations {
    /// Overall call ceiling (seconds). Adapter supplies; platform caps it.
    pub max_duration_sec: i64,
    pub max_duration_anchor: MaxDurationAnchor,
    pub keepalive: KeepaliveActivation,
}

impl PlatformActivations {
    /// Whether the cap is armed at route time, given the configured setup
    /// deadline (`setup_timeout_sec`, `<= 0` disabled).
    pub fn arms_cap_at_creation(&self, setup_timeout_sec: i64) -> bool {
        match self.max_duration_anchor {
            MaxDurationAnchor::Creation => true,
            MaxDurationAnchor::Answer => setup_timeout_sec <= 0,
        }
    }
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
/// this only picks how many). Wire values `ALL` / `FIRST` / `ONE_PER_VALUE`.
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
    /// Which 18x messages are relayed.
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
    pub allow: Option<Vec<String>>,
    /// Understood option tags, e.g. `["timer"]`.
    pub supported: Option<Vec<String>>,
}

/// Optional per-face capability-advertisement arm. The two faces of a
/// back-to-back UA are independent so an asymmetric bridge can advertise a
/// narrow set toward one domain and the full set toward the other; an absent
/// face advertises the stack default.
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
    pub toward_originator: Option<AdvertisedCapabilities>,
    /// Advertised on messages the stack sends toward an originated leg (b-leg).
    pub toward_originated: Option<AdvertisedCapabilities>,
}

/// Optional RFC 7315 §5.6 charging-correlation arm: the stack stamps a
/// `P-Charging-Vector` on every leg it ORIGINATES, so the records of the two
/// operators either side of it match on one identifier. A vector the originator
/// sent is relayed unchanged whether or not this arm is present — an identifier
/// re-minted mid-path breaks the correlation it exists for. A call whose
/// decision states the header for the leg's INVITE ([`StatedHeaders::states`])
/// mints none.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChargingVectorFeature {
    /// The element the identifier is generated at (`icid-generated-at`).
    /// Absent — the stack's own SIP address.
    pub generated_at: Option<String>,
    /// The re-INVITEs the stack sends on its own behalf carry the vector of the
    /// leg they travel: the one that leg's dialog-creating INVITE carried, none
    /// where it carried none. Off — they carry none.
    #[serde(default)]
    pub in_dialog_invites: bool,
}

/// Closed feature-activation union: mandatory `platform` + optional arms.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeatureActivations {
    pub platform: PlatformActivations,
    pub refer: Option<ReferFeature>,
    pub relay_first_18x_to_180: Option<RelayFirst18xTo180Feature>,
    pub no_answer_timeout_sec: Option<i64>,
    /// Per-face `Allow`/`Supported` advertisement. Absent — and absent for one
    /// face — means "advertise the stack default there".
    pub advertise_capabilities: Option<AdvertiseCapabilitiesFeature>,
    /// RFC 7315 §5.6 charging correlation on originated legs. Absent means the
    /// stack stamps none.
    pub charging_vector: Option<ChargingVectorFeature>,
    /// Option tags the B2BUA WITHHOLDS from every leg it originates: whatever
    /// `Supported` set would ride the originated INVITE is narrowed by these
    /// tags (an emptied set drops its line) and a relayed `Require` naming
    /// one is narrowed or dropped. Independent of the 18x-downgrade
    /// strategies, which withhold on their own beside it — a declaration that
    /// never offers `100rel` leaves 18x relay untouched. The declaration is a
    /// call-lifetime LATCH:
    /// every applied route's list unions into the standing one
    /// ([`FeatureActivations::latch_call_lifetime`], run by
    /// `apply_route` and `SetFeatures`), so a failover route whose decision
    /// does not restate it cannot restore a withheld tag.
    pub withhold_option_tags: Option<Vec<String>>,
    /// The header statements the call's decision makes on the messages the
    /// stack sends, by scope ([`StatedHeaders`]); `None` — it states none.
    /// Each decision's own: a later route's features replace it, `None`
    /// included.
    pub stated_headers: Option<StatedHeaders>,
    /// The deployment leaves media legs ([`crate::LegKind::Media`]) uncharged:
    /// a media leg's INVITE carries no `P-Charging-Vector`, neither minted nor
    /// relayed, unless the decision states one for it. `false` charges a media
    /// leg like any originated leg.
    #[serde(default)]
    pub uncharged_media_legs: bool,
    /// Header names left behind on every provisional response to the
    /// originator's INVITE that the stack relays toward the originator: the
    /// decision's narrowing of relay transparency for this call, beside the
    /// deployment's relay policy. It removes a relayed line
    /// only, never one a decision states. Each decision's own: a later route's
    /// features replace it. `None` — nothing is left behind.
    #[serde(default)]
    pub withhold_on_relayed_provisionals: Option<Vec<String>>,
}

/// A decision's header statements, one set per scope, each applied to every
/// message of its scope the stack sends, the wider scope first and the
/// narrower resolved against its result ([`crate::header_update::HeaderUpdate`]).
/// No scope reaches a media leg ([`crate::LegKind::Media`]) or what the
/// transaction layer builds on its own: a `100 Trying`, a CANCEL's `200`, the
/// `487` answering the cancelled INVITE, the ACK of a non-2xx final.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatedHeaders {
    /// The initial INVITE of every leg the call originates.
    pub launched_invite: SipHeaderUpdates,
    /// Every final response to the originator's initial INVITE the stack
    /// sends; the transaction layer's own `487` is none of them.
    pub originator_finals: SipHeaderUpdates,
    /// Every message on the originator's leg and on every leg the call
    /// originates, requests and responses, in both directions. A scope above
    /// stating the same name is resolved against what this one left.
    pub every_message: SipHeaderUpdates,
    /// Legs with a set of their own, by leg id: every message of such a leg
    /// takes that set in place of the call's, for the call's whole life.
    pub legs: std::collections::BTreeMap<String, StatedHeaders>,
    /// The set this one tentatively replaced: when the call's termination
    /// begins while it is held, the call takes it back (keeping the per-leg
    /// sets), so the teardown carries it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reverts_to: Option<Box<StatedHeaders>>,
}

impl StatedHeaders {
    /// True iff no scope states anything.
    pub fn is_empty(&self) -> bool {
        self.launched_invite.is_empty()
            && self.originator_finals.is_empty()
            && self.every_message.is_empty()
            && self.legs.is_empty()
            && self.reverts_to.is_none()
    }

    /// The set `leg_id` takes: its own, else the call's.
    pub fn of_leg(&self, leg_id: &str) -> &StatedHeaders {
        self.legs.get(leg_id).unwrap_or(self)
    }

    /// True iff an originated leg's INVITE takes a statement of `name`.
    pub fn states(&self, name: &str) -> bool {
        [&self.launched_invite, &self.every_message]
            .iter()
            .any(|set| set.keys().any(|stated| stated.eq_ignore_ascii_case(name)))
    }
}

impl FeatureActivations {
    /// The call-lifetime latch, run where a route's features replace the
    /// standing ones: the withheld option tags union (a route can widen the
    /// withhold; none can restore a withheld tag). The property belongs to the
    /// call, not to the leg the declaring route dialled.
    pub fn latch_call_lifetime(&mut self, previous: Option<&FeatureActivations>) {
        self.latch_withheld_option_tags(previous);
    }

    fn latch_withheld_option_tags(&mut self, previous: Option<&FeatureActivations>) {
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
                max_duration_anchor: Default::default(),
                keepalive: KeepaliveActivation { interval_sec: 30, max_missed: 2 },
            },
            refer: None,
            relay_first_18x_to_180: None,
            no_answer_timeout_sec: None,
            advertise_capabilities: None,
            charging_vector: None,
            withhold_option_tags: withheld.map(|w| w.iter().map(|t| t.to_string()).collect()),
            stated_headers: None,
            uncharged_media_legs: false,
            withhold_on_relayed_provisionals: None,
        }
    }

    /// The withhold latch: a route that does not restate the withheld tags
    /// inherits the standing list; one that widens it keeps both, deduplicated
    /// case-insensitively (option tags are case-insensitive, RFC 3261 §7.3.1);
    /// and with nothing standing, nothing is invented.
    #[test]
    fn the_withhold_latch_unions_and_never_narrows() {
        let mut inherited = features(None);
        inherited.latch_call_lifetime(Some(&features(Some(&["100rel"]))));
        assert_eq!(
            inherited.withhold_option_tags.as_deref(),
            Some(["100rel".to_string()].as_slice())
        );

        let mut widened = features(Some(&["timer", "100REL"]));
        widened.latch_call_lifetime(Some(&features(Some(&["100rel"]))));
        assert_eq!(
            widened.withhold_option_tags.as_deref(),
            Some(["timer".to_string(), "100REL".into()].as_slice()),
        );

        let mut untouched = features(None);
        untouched.latch_call_lifetime(Some(&features(None)));
        assert_eq!(untouched.withhold_option_tags, None);
        untouched.latch_call_lifetime(None);
        assert_eq!(untouched.withhold_option_tags, None);
    }

    /// A route's stated headers are its own: a later route stating none
    /// leaves the call with none.
    #[test]
    fn the_stated_headers_are_not_latched() {
        let mut standing = features(None);
        let mut set = StatedHeaders::default();
        set.every_message.insert("X-A".into(), crate::header_update::HeaderUpdate::line("1"));
        standing.stated_headers = Some(set);
        let mut later = features(None);
        later.latch_call_lifetime(Some(&standing));
        assert_eq!(later.stated_headers, None);
    }

    #[test]
    fn states_reads_the_launched_invite_and_every_message_scopes() {
        use crate::header_update::HeaderUpdate;
        let mut set = StatedHeaders::default();
        assert!(set.is_empty());
        set.originator_finals.insert("X-Final".into(), HeaderUpdate::line("1"));
        assert!(!set.states("x-final"), "a final is no INVITE");
        set.every_message.insert("P-Charging-Vector".into(), HeaderUpdate::Remove);
        set.launched_invite.insert("X-Launch".into(), HeaderUpdate::Add(vec!["1".into()]));
        assert!(set.states("p-charging-vector") && set.states("X-LAUNCH"));
        assert!(!set.is_empty());
    }
}
