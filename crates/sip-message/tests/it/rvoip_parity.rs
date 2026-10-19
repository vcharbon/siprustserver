//! The parity oracle's differential test (ADR-0001): every message seed of the
//! no-panic lane goes through the production parser and through
//! `rvoip-sip-core`, and the two must agree on the verdict and, when both
//! accept, on every field of [`Reading`]. Each place they part is an RFC
//! question answered in [`EXPECTED`]. An unlisted difference fails, and so
//! does a listed one that no longer occurs.

use std::collections::BTreeMap;

use sip_message::parser::rvoip::{self, Reading};
use sip_message::{CustomParser, SipParser};

use crate::no_panic_lane::seeds;

/// Where the two parsers part on one seed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Difference {
    /// The production parser refuses what the oracle accepts.
    OnlyCustomRefuses,
    /// The oracle refuses what the production parser accepts.
    OnlyRvoipRefuses,
    Start,
    CallId,
    CSeq,
    FromTag,
    ToTag,
    Vias,
    Contacts,
    Routes,
    RecordRoutes,
    Body,
}

/// Who the RFC holds against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Side {
    /// A gate of the production parser beyond the grammar: it refuses what the
    /// grammar admits, or a malformed header the RFC leaves to the element.
    Gate,
    /// The oracle departs from the RFC: it accepts a message whose start line
    /// or mandatory header breaks the grammar, refuses a valid one, or reads a
    /// field wrong.
    RvoipDeviation,
    /// Both read the same value; the oracle renders it in another form the
    /// RFC compares equal.
    EquivalentForm,
}

/// One expected difference: the seed, where the parsers part, who is right,
/// and the RFC section that says so.
#[derive(Debug)]
struct Expected {
    seed: &'static str,
    difference: Difference,
    side: Side,
    rfc: &'static str,
}

const fn gate(seed: &'static str, difference: Difference, rfc: &'static str) -> Expected {
    Expected { seed, difference, side: Side::Gate, rfc }
}

const fn rvoip(seed: &'static str, difference: Difference, rfc: &'static str) -> Expected {
    Expected { seed, difference, side: Side::RvoipDeviation, rfc }
}

/// Parameters after an unbracketed From/To URI are header parameters, so the
/// tag is the header's; the oracle files them under the URI and loses it.
const ADDR_SPEC_PARAMS: &str = "RFC 3261 §20.10, §20.20, §20.39";

/// Escaping a reserved character changes a SIP URI; the oracle escapes the
/// `+`, `;` and `=` of a user part when it renders one.
const RESERVED_ESCAPE: &str = "RFC 3261 §19.1.4, §25.1 user-unreserved";

use Difference::*;

const EXPECTED: &[Expected] = &[
    // The production parser's gates.
    gate("cve/cve_2023_28098", OnlyCustomRefuses, "RFC 3261 §22.4, §25.1 credentials"),
    gate("param-gaps/fromDuplicateTag", OnlyCustomRefuses, "RFC 3261 §8.1.1.3, §19.3"),
    gate("param-gaps/requestUriPortOverflow", OnlyCustomRefuses, "RFC 3261 §19.1.1 port"),
    gate("param-gaps/viaPortOverflow", OnlyCustomRefuses, "RFC 3261 §20.42 sent-by"),
    gate("param-gaps/viaPortZero", OnlyCustomRefuses, "RFC 3261 §20.42 sent-by"),
    gate("rfc4475-valid/longValues", OnlyCustomRefuses, "RFC 3261 §8.1.1.7; RFC 4475 §3.1.1.7"),
    gate(
        "rfc4475-valid/shortTorturousInvite",
        OnlyCustomRefuses,
        "RFC 3261 §8.1.1.7; RFC 4475 §3.1.1.1",
    ),
    gate(
        "rfc4475-valid/variedTransports",
        OnlyCustomRefuses,
        "RFC 3261 §25.1 other-transport; RFC 4475 §3.1.1.10",
    ),
    // Grammar-invalid messages the oracle accepts.
    rvoip("cve/cve_2023_27599", OnlyCustomRefuses, "RFC 3261 §25.1 quoted-string"),
    rvoip(
        "ipv6/v42_ipv6NoBrackets",
        OnlyCustomRefuses,
        "RFC 3261 §25.1 IPv6reference; RFC 5118 §4.2",
    ),
    rvoip("param-gaps/fromEmptyTag", OnlyCustomRefuses, "RFC 3261 §25.1 tag-param"),
    rvoip("param-gaps/requestUriCtlInParamName", OnlyCustomRefuses, "RFC 3261 §25.1 uri-parameter"),
    rvoip("param-gaps/requestUriIpv6Unclosed", OnlyCustomRefuses, "RFC 3261 §25.1 IPv6reference"),
    rvoip("param-gaps/requestUriNoHost", OnlyCustomRefuses, "RFC 3261 §19.1.1, §25.1 hostport"),
    rvoip("param-gaps/requestUriPortTrailingGarbage", OnlyCustomRefuses, "RFC 3261 §25.1 hostport"),
    rvoip("param-gaps/viaEmptyBranch", OnlyCustomRefuses, "RFC 3261 §25.1 via-branch"),
    rvoip("param-gaps/viaPortTrailingGarbage", OnlyCustomRefuses, "RFC 3261 §25.1 sent-by"),
    rvoip(
        "rfc4475-invalid/angleBracketRequestUri",
        OnlyCustomRefuses,
        "RFC 3261 §25.1 Request-URI; RFC 4475 §3.1.2.7",
    ),
    rvoip(
        "rfc4475-invalid/extraneousSeparators",
        OnlyCustomRefuses,
        "RFC 3261 §25.1; RFC 4475 §3.1.2.1",
    ),
    rvoip(
        "rfc4475-invalid/methodMismatch",
        OnlyCustomRefuses,
        "RFC 3261 §8.1.1.5; RFC 4475 §3.1.2.17",
    ),
    rvoip(
        "rfc4475-invalid/multipleSPInRequestLine",
        OnlyCustomRefuses,
        "RFC 3261 §7.1; RFC 4475 §3.1.2.9",
    ),
    rvoip(
        "rfc4475-invalid/overlargRequestScalars",
        OnlyCustomRefuses,
        "RFC 3261 §8.1.1.5; RFC 4475 §3.1.2.4",
    ),
    rvoip(
        "rfc4475-invalid/overlargResponseScalars",
        OnlyCustomRefuses,
        "RFC 3261 §8.1.1.5; RFC 4475 §3.1.2.5",
    ),
    rvoip(
        "rfc4475-invalid/spacesInAddrSpec",
        OnlyCustomRefuses,
        "RFC 3261 §25.1 addr-spec; RFC 4475 §3.1.2.14",
    ),
    rvoip(
        "rfc4475-invalid/unknownMethodMismatch",
        OnlyCustomRefuses,
        "RFC 3261 §8.1.1.5; RFC 4475 §3.1.2.18",
    ),
    rvoip(
        "rfc4475-invalid/unknownProtocolVersion",
        OnlyCustomRefuses,
        "RFC 3261 §7.1; RFC 4475 §3.1.2.16",
    ),
    rvoip(
        "rfc4475-invalid/unterminatedQuotedString",
        OnlyCustomRefuses,
        "RFC 3261 §25.1 quoted-string; RFC 4475 §3.1.2.6",
    ),
    // Valid messages the oracle refuses.
    rvoip(
        "rfc4475-valid/emptyReasonPhrase",
        OnlyRvoipRefuses,
        "RFC 3261 §25.1 Reason-Phrase; RFC 4475 §3.1.1.13",
    ),
    rvoip(
        "rfc4475-valid/extraTrailingOctets",
        OnlyRvoipRefuses,
        "RFC 3261 §18.3; RFC 4475 §3.1.1.8",
    ),
    rvoip("rfc4475-valid/multipartMime", OnlyRvoipRefuses, "RFC 3261 §7.4; RFC 4475 §3.1.1.11"),
    rvoip(
        "rfc4475-valid/wideRangeValidChars",
        OnlyRvoipRefuses,
        "RFC 3261 §25.1; RFC 4475 §3.1.1.2",
    ),
    // The tag of an unbracketed From/To URI.
    rvoip("ipv6/v410a_extraColon", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v410b_correctIpv4InIpv6", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v41_basicIpv6", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v43_portAmbiguousInBrackets", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v44_portUnambiguous", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v45a_receivedBracketed", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v45a_receivedBracketed", ToTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v45b_receivedBare", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v46_ipv6InSdp", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v47_mixedIpv4Ipv6Vias", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v47_mixedIpv4Ipv6Vias", ToTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v48_multipleIpInSdp", FromTag, ADDR_SPEC_PARAMS),
    rvoip("ipv6/v49_ipv4Mapped", FromTag, ADDR_SPEC_PARAMS),
    rvoip("rfc4475-invalid/escapedHeadersInUri", FromTag, ADDR_SPEC_PARAMS),
    rvoip("rfc4475-invalid/invalidTimezone", FromTag, ADDR_SPEC_PARAMS),
    rvoip("rfc4475-invalid/unencloseNameAddr", FromTag, ADDR_SPEC_PARAMS),
    rvoip("rfc4475-valid/escapedNulls", FromTag, ADDR_SPEC_PARAMS),
    rvoip("rfc4475-valid/semicolonInUserPart", FromTag, ADDR_SPEC_PARAMS),
    rvoip("rfc4475-valid/unusualReasonPhrase", FromTag, ADDR_SPEC_PARAMS),
    rvoip("rfc4475-valid/unusualReasonPhrase", ToTag, ADDR_SPEC_PARAMS),
    // A mandatory From the oracle cannot read, in a message it accepts.
    rvoip("rfc4475-invalid/nonTokenDisplayName", FromTag, "RFC 3261 §8.1.1.3; RFC 4475 §3.1.2.15"),
    rvoip("synthetic/utf8-display", FromTag, "RFC 3261 §8.1.1.3"),
    // A Contact entry the oracle drops from a message it accepts.
    rvoip("rfc4475-invalid/unencloseNameAddr", Contacts, "RFC 3261 §20.10; RFC 4475 §3.1.2.13"),
    // A bare IPv6 `received` the oracle stops at, losing the branch after it.
    rvoip("ipv6/v45b_receivedBare", Vias, "RFC 3261 §20.42 via-received; RFC 5118 §4.5"),
    // Request-URIs the oracle renders changed.
    rvoip("rfc4475-valid/semicolonInUserPart", Start, RESERVED_ESCAPE),
    rvoip("synthetic/cancel", Start, RESERVED_ESCAPE),
    rvoip("synthetic/invite-sdp", Start, RESERVED_ESCAPE),
    Expected {
        seed: "ipv6/v410b_correctIpv4InIpv6",
        difference: Start,
        side: Side::EquivalentForm,
        rfc: "RFC 3261 §19.1.4; RFC 5118 §4.10",
    },
];

/// Every field of the two readings that differs, with both values.
fn differences(
    custom: &Result<Reading, String>,
    oracle: &Result<Reading, String>,
) -> Vec<(Difference, String)> {
    let (c, r) = match (custom, oracle) {
        (Err(_), Err(_)) => return Vec::new(),
        (Err(why), Ok(_)) => return vec![(OnlyCustomRefuses, why.clone())],
        (Ok(_), Err(why)) => return vec![(OnlyRvoipRefuses, why.clone())],
        (Ok(c), Ok(r)) => (c, r),
    };
    let fields: [(Difference, bool, String); 10] = [
        (Start, c.start == r.start, format!("{:?} / {:?}", c.start, r.start)),
        (CallId, c.call_id == r.call_id, format!("{:?} / {:?}", c.call_id, r.call_id)),
        (CSeq, c.cseq == r.cseq, format!("{:?} / {:?}", c.cseq, r.cseq)),
        (FromTag, c.from_tag == r.from_tag, format!("{:?} / {:?}", c.from_tag, r.from_tag)),
        (ToTag, c.to_tag == r.to_tag, format!("{:?} / {:?}", c.to_tag, r.to_tag)),
        (Vias, c.vias == r.vias, format!("{:?} / {:?}", c.vias, r.vias)),
        (Contacts, c.contacts == r.contacts, format!("{} / {}", c.contacts, r.contacts)),
        (Routes, c.routes == r.routes, format!("{:?} / {:?}", c.routes, r.routes)),
        (
            RecordRoutes,
            c.record_routes == r.record_routes,
            format!("{:?} / {:?}", c.record_routes, r.record_routes),
        ),
        (Body, c.body == r.body, format!("{} / {} bytes", c.body.len(), r.body.len())),
    ];
    fields.into_iter().filter(|(_, same, _)| !same).map(|(d, _, detail)| (d, detail)).collect()
}

#[test]
fn oracle_agrees_but_for_listed_differences() {
    let parser = CustomParser::new();
    let mut found: BTreeMap<(String, Difference), String> = BTreeMap::new();
    for seed in seeds::messages() {
        let custom = parser.parse(&seed.bytes).map(|m| Reading::of(&m)).map_err(|e| e.reason);
        let oracle = rvoip::read(&seed.bytes);
        for (d, detail) in differences(&custom, &oracle) {
            found.insert((seed.name.clone(), d), detail);
        }
    }
    let mut failures = Vec::new();
    for ((seed, d), detail) in &found {
        if !EXPECTED.iter().any(|e| e.seed == seed && e.difference == *d) {
            failures.push(format!("  unlisted {seed} {d:?}: {detail} (production / oracle)"));
        }
    }
    for e in EXPECTED {
        if !found.contains_key(&(e.seed.to_string(), e.difference)) {
            failures.push(format!("  no longer differs: {e:?}"));
        }
    }
    assert!(failures.is_empty(), "parity oracle differences:\n{}", failures.join("\n"));
}

/// Each entry names its RFC section once, and its side fits its difference: a
/// gate only refuses, an equivalent form is a field both parsers read.
#[test]
fn listed_differences_are_well_formed() {
    let mut seen = BTreeMap::new();
    for e in EXPECTED {
        assert!(e.rfc.starts_with("RFC "), "no RFC section: {e:?}");
        let fits = match e.side {
            Side::Gate => e.difference == OnlyCustomRefuses,
            Side::RvoipDeviation => true,
            Side::EquivalentForm => !matches!(e.difference, OnlyCustomRefuses | OnlyRvoipRefuses),
        };
        assert!(fits, "side does not fit the difference: {e:?}");
        assert!(seen.insert((e.seed, e.difference), ()).is_none(), "listed twice: {e:?}");
    }
}
