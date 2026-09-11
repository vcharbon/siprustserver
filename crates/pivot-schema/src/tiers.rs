//! The NORMATIVE DATA the pivot's tier model rests on
//! (`PCAP2TEST_PIVOT_V3.md` §8.2): the tier-1 omission list, per-message
//! address-header handling, the compact-form header identity map and the
//! multipart body-handling registry.
//!
//! Exported as DATA, not prose, so a generator and an interpreter consume one
//! list instead of each reverse-engineering it. The compact-form map is
//! enumerated from `sip-message`, the one crate that owns header identity —
//! this file never restates it.
//!
//! The body registry is the GENERIC arm only. A deployment that rewrites
//! numbers inside its own body format registers that content type in its own
//! overlay; a registry naming one operator's media types would not be a
//! contract, it would be a leak.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The four lists, as one exportable document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TierData {
    /// Headers a stored message spec never carries.
    pub tier1_omitted: Vec<OmittedHeader>,
    /// What happens to the address headers, per message.
    pub address_handling: Vec<AddressHandling>,
    /// RFC 3261 §7.3.3 compact names and what they expand to.
    pub compact_forms: Vec<CompactForm>,
    /// Per-content-type body handling.
    pub body_registry: BodyRegistry,
}

/// One tier-1 header and why a stored spec omits it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OmittedHeader {
    /// Canonical header name. A captured compact form names the same header, so
    /// the list states each name once.
    pub name: String,
    /// Why a stored spec omits it.
    pub reason: OmissionReason,
}

/// Why a header is absent from a stored message spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum OmissionReason {
    /// The interpreter's stack regenerates it from dialog state.
    Regenerated,
    /// It is represented elsewhere in the document, in a tier-2 or body field.
    RepresentedElsewhere,
}

/// How one address header is treated, per message class.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddressHandling {
    /// Header name, or `R-URI` for the request line's target.
    pub field: String,
    /// The message class this row applies to.
    pub on: MessageClass,
    /// Which tier the field lands in for that class.
    pub handling: AddressTier,
}

/// Which messages an address-handling row governs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum MessageClass {
    /// The INVITE that OPENS a dialog, emitted by the actor. A `dir: in` leg's
    /// opener is its RECEIVED INVITE, so an emitted INVITE there is a
    /// re-INVITE and never initial.
    DialogOpeningInviteSend,
    /// Every other message, in-dialog INVITEs included.
    Otherwise,
}

/// The tier an address field lands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum AddressTier {
    /// Stored as a tier-2 ref: role-mapped, or frozen where the plan does not
    /// recognize the value.
    Tier2Ref,
    /// The stack regenerates it; nothing is stored.
    Tier1Regenerated,
    /// The stack regenerates host:port and preserves the rest.
    Tier1HostPort,
}

/// One compact header name and its canonical expansion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactForm {
    /// The one-letter compact name.
    pub compact: String,
    /// The header it names.
    pub canonical: String,
}

/// The per-content-type body handling registry. Rules are tried IN ORDER; the
/// first match wins, and `unmatched` covers what none claims.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BodyRegistry {
    /// The media type is lowercased and its parameters stripped before matching.
    pub rules: Vec<BodyRule>,
    /// What happens to a media type no rule claims.
    pub unmatched: UnmatchedHandling,
}

/// One registry rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BodyRule {
    /// How the rule recognizes a media type.
    #[serde(rename = "match")]
    pub match_on: MediaTypeMatch,
    /// What it does with a body or part that matched.
    pub handling: BodyHandling,
}

/// How a rule recognizes a media type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "by", rename_all = "kebab-case", deny_unknown_fields)]
pub enum MediaTypeMatch {
    Exact { value: String },
    Prefix { value: String },
    Suffix { value: String },
}

impl MediaTypeMatch {
    /// Whether `media_type` (already lowercased, parameters stripped) matches.
    pub fn matches(&self, media_type: &str) -> bool {
        match self {
            MediaTypeMatch::Exact { value } => media_type == value,
            MediaTypeMatch::Prefix { value } => media_type.starts_with(value.as_str()),
            MediaTypeMatch::Suffix { value } => media_type.ends_with(value.as_str()),
        }
    }
}

/// What the registry does with a matched body or part.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum BodyHandling {
    /// Apply these rewrite tokens before emission.
    Rewrite { tokens: Vec<String> },
    /// Replay byte-exact.
    Freeze { mode: crate::body::BodyMode },
}

/// What happens to a media type no rule claims. A missing handler is a decision
/// OWED, not a silent freeze: the part is frozen so the replay is faithful, and
/// flagged so a human chooses between freezing it and writing a handler.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct UnmatchedHandling {
    /// What the part gets, absent a handler: frozen, so the replay is faithful.
    pub handling: BodyHandling,
    /// Flag kind raised on the case.
    pub flag: String,
    /// Flag only when the payload carries at least this many ASCII digits —
    /// the evidence that it may embed a number the replay must rewrite.
    pub flag_when_digits_at_least: usize,
}

/// Build the export. Pure data plus one enumeration of `sip-message`'s table.
pub fn tier_data() -> TierData {
    TierData {
        tier1_omitted: tier1_omitted(),
        address_handling: address_handling(),
        compact_forms: compact_forms(),
        body_registry: body_registry(),
    }
}

fn tier1_omitted() -> Vec<OmittedHeader> {
    use OmissionReason::{Regenerated, RepresentedElsewhere};
    [
        ("Via", Regenerated),
        ("Call-ID", Regenerated),
        ("CSeq", Regenerated),
        ("Max-Forwards", Regenerated),
        ("Contact", Regenerated),
        ("Route", Regenerated),
        ("Record-Route", Regenerated),
        ("From", RepresentedElsewhere),
        ("To", RepresentedElsewhere),
        ("Content-Type", RepresentedElsewhere),
        ("Content-Length", RepresentedElsewhere),
    ]
    .into_iter()
    .map(|(name, reason)| OmittedHeader { name: name.to_string(), reason })
    .collect()
}

fn address_handling() -> Vec<AddressHandling> {
    use AddressTier::{Tier1HostPort, Tier1Regenerated, Tier2Ref};
    use MessageClass::{DialogOpeningInviteSend, Otherwise};
    [
        ("R-URI", DialogOpeningInviteSend, Tier2Ref),
        // In-dialog, the R-URI IS the learned remote target.
        ("R-URI", Otherwise, Tier1Regenerated),
        ("From", DialogOpeningInviteSend, Tier2Ref),
        ("From", Otherwise, Tier1Regenerated),
        ("To", DialogOpeningInviteSend, Tier2Ref),
        ("To", Otherwise, Tier1Regenerated),
        // Contact is never role-mapped: its routing-critical part is host:port,
        // which only the bound socket knows.
        ("Contact", DialogOpeningInviteSend, Tier1HostPort),
        ("Contact", Otherwise, Tier1HostPort),
    ]
    .into_iter()
    .map(|(field, on, handling)| AddressHandling { field: field.to_string(), on, handling })
    .collect()
}

/// Enumerate the compact forms by probing `sip-message` over the ASCII letters.
/// The table lives there and only there.
fn compact_forms() -> Vec<CompactForm> {
    ('a'..='z')
        .filter_map(|c| {
            let compact = c.to_string();
            sip_message::compact_form_canonical(&compact)
                .map(|canonical| CompactForm { compact, canonical: canonical.to_string() })
        })
        .collect()
}

fn body_registry() -> BodyRegistry {
    let freeze = |mode| BodyHandling::Freeze { mode };
    let frozen = freeze(crate::body::BodyMode::Frozen);
    BodyRegistry {
        rules: vec![
            BodyRule {
                match_on: MediaTypeMatch::Exact { value: "application/sdp".into() },
                handling: BodyHandling::Rewrite { tokens: vec!["c=addr".into(), "m=port".into()] },
            },
            BodyRule {
                match_on: MediaTypeMatch::Exact {
                    value: "application/emergencycalldata.ecall.msd".into(),
                },
                handling: freeze(crate::body::BodyMode::FrozenBinary),
            },
            BodyRule {
                match_on: MediaTypeMatch::Prefix { value: "application/emergencycalldata.".into() },
                handling: frozen.clone(),
            },
            BodyRule {
                match_on: MediaTypeMatch::Suffix { value: "+xml".into() },
                handling: frozen.clone(),
            },
            BodyRule {
                match_on: MediaTypeMatch::Prefix { value: "text/".into() },
                handling: frozen.clone(),
            },
        ],
        unmatched: UnmatchedHandling {
            handling: frozen,
            flag: "unrecognized-body-part".into(),
            flag_when_digits_at_least: 5,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_compact_form_map_is_the_full_rfc_3261_table() {
        let forms = compact_forms();
        assert_eq!(forms.len(), 10, "{forms:?}");
        for (compact, canonical) in [
            ("i", "Call-ID"),
            ("m", "Contact"),
            ("e", "Content-Encoding"),
            ("l", "Content-Length"),
            ("c", "Content-Type"),
            ("f", "From"),
            ("s", "Subject"),
            ("k", "Supported"),
            ("t", "To"),
            ("v", "Via"),
        ] {
            let found = forms.iter().find(|f| f.compact == compact).expect(compact);
            assert_eq!(found.canonical, canonical);
        }
    }

    #[test]
    fn every_tier1_omission_and_compact_form_agree_on_header_identity() {
        // A compact form must never widen the omission list: the canonical name
        // it expands to is either omitted or not, and stating the compact name
        // separately would let one spelling through.
        let omitted: Vec<String> = tier1_omitted().into_iter().map(|h| h.name).collect();
        for form in compact_forms() {
            assert!(
                !omitted.contains(&form.compact),
                "the omission list states the compact form {:?} as well as its canonical name",
                form.compact
            );
        }
    }

    #[test]
    fn the_registry_resolves_the_body_types_the_corpus_carries() {
        let registry = body_registry();
        let handling = |ct: &str| {
            registry
                .rules
                .iter()
                .find(|r| r.match_on.matches(ct))
                .map(|r| r.handling.clone())
                .unwrap_or_else(|| registry.unmatched.handling.clone())
        };
        assert!(matches!(handling("application/sdp"), BodyHandling::Rewrite { .. }));
        assert_eq!(
            handling("application/emergencycalldata.ecall.msd"),
            BodyHandling::Freeze { mode: crate::body::BodyMode::FrozenBinary }
        );
        assert_eq!(
            handling("application/mscp+xml"),
            BodyHandling::Freeze { mode: crate::body::BodyMode::Frozen }
        );
        assert_eq!(
            handling("text/plain"),
            BodyHandling::Freeze { mode: crate::body::BodyMode::Frozen }
        );
        // Unrecognized: frozen, and flagged — a missing handler is a decision owed.
        assert_eq!(handling("application/octet-stream"), registry.unmatched.handling);
    }

    #[test]
    fn the_specific_ecall_rule_wins_over_the_family_prefix() {
        let registry = body_registry();
        let first = registry
            .rules
            .iter()
            .find(|r| r.match_on.matches("application/emergencycalldata.ecall.msd"))
            .unwrap();
        assert_eq!(
            first.handling,
            BodyHandling::Freeze { mode: crate::body::BodyMode::FrozenBinary }
        );
    }

    #[test]
    fn the_export_round_trips_through_its_canonical_form() {
        let text = crate::canonical::format(&tier_data()).unwrap();
        let back: TierData = serde_json::from_str(&text).unwrap();
        assert_eq!(back, tier_data());
    }
}
