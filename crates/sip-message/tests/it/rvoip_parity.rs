//! The parity oracle's differential test (ADR-0001): every message seed of the
//! no-panic lane goes through the production parser and through
//! `rvoip-sip-core`, and the two must agree on the verdict and, when both
//! accept, on every field of [`Reading`]. Each place they part is an RFC
//! question answered in [`EXPECTED`], down to a digest of both sides' values.
//! An unlisted difference fails, so does a listed one whose values changed,
//! and so does one that no longer occurs.

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
    /// A gate of the production parser beyond the grammar (ADR-0001): it
    /// refuses what the grammar admits, or a malformed header the RFC leaves
    /// to the element.
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
/// the RFC section that says so, and the [`digest`] of both sides' values.
#[derive(Debug)]
struct Expected {
    seed: &'static str,
    difference: Difference,
    side: Side,
    rfc: &'static str,
    values: u64,
}

impl Expected {
    const fn values(self, values: u64) -> Self {
        Self { values, ..self }
    }
}

const fn gate(seed: &'static str, difference: Difference, rfc: &'static str) -> Expected {
    Expected { seed, difference, side: Side::Gate, rfc, values: 0 }
}

const fn rvoip(seed: &'static str, difference: Difference, rfc: &'static str) -> Expected {
    Expected { seed, difference, side: Side::RvoipDeviation, rfc, values: 0 }
}

/// The cookie-less top Via of an RFC 2543 element, refused by scope choice.
const COOKIE: &str = "RFC 3261 §8.1.1.7, departing from §17.2.3";

/// Parameters after an unbracketed From/To URI are header parameters, so the
/// tag is the header's; the oracle files them under the URI and loses it.
const ADDR_SPEC_PARAMS: &str = "RFC 3261 §20.10, §20.20, §20.39";

/// Escaping a reserved character changes a SIP URI; the oracle escapes the
/// `+`, `;` and `=` of a user part when it renders one.
const RESERVED_ESCAPE: &str = "RFC 3261 §19.1.4, §25.1 user-unreserved";

use Difference::*;

const EXPECTED: &[Expected] = &[
    // The production parser's gates.
    gate("cve/cve_2023_28098", OnlyCustomRefuses, "RFC 3261 §22.4, §25.1 credentials")
        .values(0xc16a9a4cde43c1fb),
    gate("param-gaps/fromDuplicateTag", OnlyCustomRefuses, "RFC 3261 §8.1.1.3, §19.3")
        .values(0xc65c90c33e9b74d9),
    gate("param-gaps/requestUriPortOverflow", OnlyCustomRefuses, "RFC 3261 §19.1.1 port")
        .values(0xbd21100f3778e6f1),
    gate("param-gaps/viaPortOverflow", OnlyCustomRefuses, "RFC 3261 §20.42 sent-by")
        .values(0x2688719069804f87),
    gate("param-gaps/viaPortZero", OnlyCustomRefuses, "RFC 3261 §18.2.2, §20.42 sent-by")
        .values(0xa1d4db8865a6faa8),
    gate("rfc4475-valid/longValues", OnlyCustomRefuses, COOKIE).values(0x59d4b41e95b6aa85),
    gate("rfc4475-valid/shortTorturousInvite", OnlyCustomRefuses, COOKIE)
        .values(0x319c73e2eb7957a0),
    // Grammar-invalid messages the oracle accepts.
    rvoip("cve/cve_2023_27599", OnlyCustomRefuses, "RFC 3261 §25.1 quoted-string")
        .values(0x5375cbaccf51bcc2),
    rvoip("ipv6/v42_ipv6NoBrackets", OnlyCustomRefuses, "RFC 3261 §25.1; RFC 5118 §4.2")
        .values(0xb6843c68f7c98e91),
    rvoip("param-gaps/fromEmptyTag", OnlyCustomRefuses, "RFC 3261 §25.1 tag-param")
        .values(0xf08f087dcdf891df),
    rvoip("param-gaps/requestUriCtlInParamName", OnlyCustomRefuses, "RFC 3261 §25.1")
        .values(0x2728cd06adea20af),
    rvoip("param-gaps/requestUriIpv6Unclosed", OnlyCustomRefuses, "RFC 3261 §25.1")
        .values(0x9bd7c5c92f3ead99),
    rvoip("param-gaps/requestUriNoHost", OnlyCustomRefuses, "RFC 3261 §19.1.1, §25.1")
        .values(0xd8b1ca963fae2749),
    rvoip("param-gaps/requestUriPortTrailingGarbage", OnlyCustomRefuses, "RFC 3261 §25.1")
        .values(0x679fd0f32d4672d3),
    rvoip("param-gaps/viaEmptyBranch", OnlyCustomRefuses, "RFC 3261 §25.1 via-branch")
        .values(0x57f029553723be25),
    rvoip("param-gaps/viaPortTrailingGarbage", OnlyCustomRefuses, "RFC 3261 §25.1 sent-by")
        .values(0xe7f8970a3c605f11),
    rvoip("rfc4475-invalid/angleBracketRequestUri", OnlyCustomRefuses, "RFC 4475 §3.1.2.7")
        .values(0x377fef051d494302),
    rvoip("rfc4475-invalid/escapedHeadersInUri", OnlyCustomRefuses, "RFC 3261 §19.1.1 Table 1")
        .values(0x6752bdbce2491d00),
    rvoip("rfc4475-invalid/nonTokenDisplayName", OnlyCustomRefuses, "RFC 4475 §3.1.2.15")
        .values(0xde535a809e338700),
    rvoip("rfc4475-invalid/unencloseNameAddr", OnlyCustomRefuses, "RFC 4475 §3.1.2.13")
        .values(0x01a87ce68e28faa0),
    rvoip("synthetic/utf8-raw-user", OnlyCustomRefuses, "RFC 3261 §25.1 user, escaped")
        .values(0xc658e4784a248ef3),
    rvoip("rfc4475-invalid/extraneousSeparators", OnlyCustomRefuses, "RFC 4475 §3.1.2.1")
        .values(0x921cff418a09fd67),
    rvoip("rfc4475-invalid/methodMismatch", OnlyCustomRefuses, "RFC 3261 §8.1.1.5")
        .values(0xf207efadaf6b4552),
    rvoip("rfc4475-invalid/multipleSPInRequestLine", OnlyCustomRefuses, "RFC 3261 §7.1")
        .values(0x0afaaed9c35e8ba7),
    rvoip("rfc4475-invalid/overlargRequestScalars", OnlyCustomRefuses, "RFC 3261 §8.1.1.5")
        .values(0x9133466b544daf7f),
    rvoip("rfc4475-invalid/overlargResponseScalars", OnlyCustomRefuses, "RFC 3261 §8.1.1.5")
        .values(0xf3c074caf38e0f5d),
    rvoip("rfc4475-invalid/spacesInAddrSpec", OnlyCustomRefuses, "RFC 4475 §3.1.2.14")
        .values(0x6437d54bb7592a36),
    rvoip("rfc4475-invalid/unknownMethodMismatch", OnlyCustomRefuses, "RFC 3261 §8.1.1.5")
        .values(0xd4f1563f06b7c76d),
    rvoip("rfc4475-invalid/unknownProtocolVersion", OnlyCustomRefuses, "RFC 3261 §7.1")
        .values(0xfe0e9076ca2e055a),
    rvoip("rfc4475-invalid/unterminatedQuotedString", OnlyCustomRefuses, "RFC 4475 §3.1.2.6")
        .values(0x5375cbaccf51bcc2),
    // Valid messages the oracle refuses.
    rvoip("rfc4475-valid/emptyReasonPhrase", OnlyRvoipRefuses, "RFC 4475 §3.1.1.13")
        .values(0x768d41a8cf8b34aa),
    rvoip("rfc4475-valid/extraTrailingOctets", OnlyRvoipRefuses, "RFC 3261 §18.3")
        .values(0x768d41a8cf8b34aa),
    rvoip("rfc4475-valid/multipartMime", OnlyRvoipRefuses, "RFC 4475 §3.1.1.11")
        .values(0x768d41a8cf8b34aa),
    // The tag of an unbracketed From/To URI.
    rvoip("ipv6/v410a_extraColon", FromTag, ADDR_SPEC_PARAMS).values(0xa0f4a29e4b2e58f8),
    rvoip("ipv6/v410b_correctIpv4InIpv6", FromTag, ADDR_SPEC_PARAMS).values(0xa0f4a29e4b2e58f8),
    rvoip("ipv6/v41_basicIpv6", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v43_portAmbiguousInBrackets", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v44_portUnambiguous", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v45a_receivedBracketed", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v45a_receivedBracketed", ToTag, ADDR_SPEC_PARAMS).values(0x7cd0c0f47b606388),
    rvoip("ipv6/v45b_receivedBare", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v46_ipv6InSdp", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v47_mixedIpv4Ipv6Vias", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v47_mixedIpv4Ipv6Vias", ToTag, ADDR_SPEC_PARAMS).values(0xd24dd170bd8db175),
    rvoip("ipv6/v48_multipleIpInSdp", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("ipv6/v49_ipv4Mapped", FromTag, ADDR_SPEC_PARAMS).values(0xec9681ef8a5b85d8),
    rvoip("rfc4475-invalid/invalidTimezone", FromTag, ADDR_SPEC_PARAMS).values(0xfcf9cfef20cb724e),
    rvoip("rfc4475-valid/escapedNulls", FromTag, ADDR_SPEC_PARAMS).values(0x8146bcc924d85594),
    rvoip("rfc4475-valid/semicolonInUserPart", FromTag, ADDR_SPEC_PARAMS)
        .values(0x1761762f9f70ab1f),
    rvoip("rfc4475-valid/unusualReasonPhrase", FromTag, ADDR_SPEC_PARAMS)
        .values(0x3fb5b3df1e6604b5),
    rvoip("rfc4475-valid/unusualReasonPhrase", ToTag, ADDR_SPEC_PARAMS).values(0x5b12eb6bf24b8138),
    // A bare IPv6 `received` the oracle stops at, losing the branch after it.
    rvoip("ipv6/v45b_receivedBare", Vias, "RFC 3261 §20.42; RFC 5118 §4.5")
        .values(0x6c0069c1c9db60a1),
    // Request-URIs the oracle renders changed.
    rvoip("rfc4475-valid/semicolonInUserPart", Start, RESERVED_ESCAPE).values(0x03804767341bed84),
    rvoip("rfc4475-valid/wideRangeValidChars", Start, RESERVED_ESCAPE).values(0x1b2e91773d7da20a),
    rvoip("synthetic/cancel", Start, RESERVED_ESCAPE).values(0x35b690fec65f2844),
    rvoip("synthetic/invite-sdp", Start, RESERVED_ESCAPE).values(0xa009737edb4ccd12),
    Expected {
        seed: "ipv6/v410b_correctIpv4InIpv6",
        difference: Start,
        side: Side::EquivalentForm,
        rfc: "RFC 3261 §19.1.4; RFC 5118 §4.10",
        values: 0x62895373da78f08f,
    },
];

/// FNV-1a over `text`: the stable digest an entry pins both sides' values by.
fn digest(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
}

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
        let now = digest(detail);
        match EXPECTED.iter().find(|e| e.seed == seed && e.difference == *d) {
            None => failures.push(format!("  unlisted {seed} {d:?} {now:#018x}: {detail}")),
            Some(e) if e.values != now => {
                failures.push(format!("  values changed {seed} {d:?} {now:#018x}: {detail}"))
            }
            Some(_) => {}
        }
    }
    for e in EXPECTED {
        if !found.contains_key(&(e.seed.to_string(), e.difference)) {
            failures.push(format!("  no longer differs: {e:?}"));
        }
    }
    assert!(
        failures.is_empty(),
        "parity oracle differences (production / oracle):\n{}",
        failures.join("\n")
    );
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
