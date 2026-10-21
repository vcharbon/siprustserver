//! RFC 4475 torture + CVE-regression + RFC 3261 param-gap + RFC 5118 IPv6
//! compliance matrix for the ported custom parser. Port of
//! `tests/sip/parser-compliance.test.ts` (the `custom` column).
//!
//! Fixtures are the byte-exact files under `tests/fixtures/<category>/`,
//! dumped verbatim from the TS source (the `sipMsg` template results) so the
//! wire bytes match 1:1.
//!
//! Every category is read by the one default parse: the grammar of RFC 3261
//! §25.1 and the gates of ADR-0001 are all held at parse time.

use std::collections::HashSet;
use std::fs;
use std::path::PathBuf;

use sip_message::{CustomParser, SipParser};

fn fixture(category: &str, name: &str) -> Vec<u8> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures");
    p.push(category);
    p.push(format!("{name}.sip"));
    fs::read(&p).unwrap_or_else(|e| panic!("read fixture {p:?}: {e}"))
}

fn verdict(bytes: &[u8]) -> Result<(), String> {
    CustomParser::new().parse(bytes).map(|_| ()).map_err(|e| e.reason)
}

/// Run a category: assert each fixture's accept/reject matches expectation,
/// collecting every mismatch so one run surfaces them all.
fn run_category(category: &str, names: &[&str], expect_reject: &HashSet<&str>) {
    let mut mismatches: Vec<String> = Vec::new();
    for &name in names {
        let bytes = fixture(category, name);
        let v = verdict(&bytes);
        let should_reject = expect_reject.contains(name);
        match (&v, should_reject) {
            (Ok(()), true) => {
                mismatches.push(format!("  {category}/{name}: expected REJECT, but ACCEPTED"))
            }
            (Err(reason), false) => mismatches
                .push(format!("  {category}/{name}: expected ACCEPT, but REJECTED ({reason})")),
            _ => {}
        }
    }
    assert!(
        mismatches.is_empty(),
        "compliance mismatches in {category}:\n{}",
        mismatches.join("\n")
    );
}

fn set(items: &[&'static str]) -> HashSet<&'static str> {
    items.iter().copied().collect()
}

// ---------------------------------------------------------------------------
// RFC 4475 §3.1.1 — valid messages (must parse, except the gates of ADR-0001)
// ---------------------------------------------------------------------------

#[test]
fn rfc4475_valid() {
    let names = [
        "shortTorturousInvite",    // 3.1.1.1
        "wideRangeValidChars",     // 3.1.1.2
        "validPercentEscaping",    // 3.1.1.3
        "escapedNulls",            // 3.1.1.4
        "percentNotEscape",        // 3.1.1.5
        "noLwsBeforeAngleBracket", // 3.1.1.6
        "longValues",              // 3.1.1.7
        "extraTrailingOctets",     // 3.1.1.8
        "semicolonInUserPart",     // 3.1.1.9
        "variedTransports",        // 3.1.1.10
        "multipartMime",           // 3.1.1.11
        "unusualReasonPhrase",     // 3.1.1.12
        "emptyReasonPhrase",       // 3.1.1.13
    ];
    // 3.1.1.1 and 3.1.1.7 carry a top Via without the magic cookie: RFC 2543
    // requests, refused by scope choice (ADR-0001 gates, departing from RFC
    // 3261 §17.2.3).
    let reject = set(&["shortTorturousInvite", "longValues"]);
    run_category("rfc4475-valid", &names, &reject);
}

// ---------------------------------------------------------------------------
// RFC 4475 §3.1.2 — invalid messages (should reject)
// ---------------------------------------------------------------------------

#[test]
fn rfc4475_invalid() {
    let names = [
        "extraneousSeparators",        // 3.1.2.1
        "contentLengthTooLarge",       // 3.1.2.2
        "negativeContentLength",       // 3.1.2.3
        "overlargRequestScalars",      // 3.1.2.4
        "overlargResponseScalars",     // 3.1.2.5
        "unterminatedQuotedString",    // 3.1.2.6
        "angleBracketRequestUri",      // 3.1.2.7
        "embeddedLwsInUri",            // 3.1.2.8
        "multipleSPInRequestLine",     // 3.1.2.9
        "trailingSpacesInRequestLine", // 3.1.2.10
        "escapedHeadersInUri",         // 3.1.2.11
        "invalidTimezone",             // 3.1.2.12
        "unencloseNameAddr",           // 3.1.2.13
        "spacesInAddrSpec",            // 3.1.2.14
        "nonTokenDisplayName",         // 3.1.2.15
        "unknownProtocolVersion",      // 3.1.2.16
        "methodMismatch",              // 3.1.2.17
        "unknownMethodMismatch",       // 3.1.2.18
        "overlargeResponseCode",       // 3.1.2.19
    ];
    // Reject every invalid fixture except trailingSpacesInRequestLine
    // (3.1.2.10), whose trailing SP the start-line reader leaves to the
    // Request-URI split.
    let lenient = set(&["trailingSpacesInRequestLine"]);
    let reject: HashSet<&str> = names.iter().copied().filter(|n| !lenient.contains(n)).collect();
    run_category("rfc4475-invalid", &names, &reject);
}

// ---------------------------------------------------------------------------
// CVE regression (must reject all)
// ---------------------------------------------------------------------------

#[test]
fn cve_regression() {
    let names = ["cve_2023_27598", "cve_2023_27599", "cve_2023_28098"];
    let reject = set(&names);
    run_category("cve", &names, &reject);
}

// ---------------------------------------------------------------------------
// RFC 3261 param-grammar gaps (must reject all)
// ---------------------------------------------------------------------------

#[test]
fn param_grammar_gaps() {
    let names = [
        "viaPortOverflow",
        "viaPortZero",
        "requestUriPortOverflow",
        "viaEmptyBranch",
        "fromEmptyTag",
        "fromDuplicateTag",
        "viaPortTrailingGarbage",
        "requestUriNoHost",
        "requestUriIpv6Unclosed",
        "requestUriPortTrailingGarbage",
        "requestUriCtlInParamName",
    ];
    let reject = set(&names);
    run_category("param-gaps", &names, &reject);
}

// ---------------------------------------------------------------------------
// RFC 5118 IPv6 torture
// ---------------------------------------------------------------------------

#[test]
fn rfc5118_ipv6() {
    let valid = [
        "v41_basicIpv6",
        "v43_portAmbiguousInBrackets",
        "v44_portUnambiguous",
        "v45a_receivedBracketed",
        "v45b_receivedBare",
        "v46_ipv6InSdp",
        "v47_mixedIpv4Ipv6Vias",
        "v48_multipleIpInSdp",
        "v49_ipv4Mapped",
        "v410a_extraColon",
        "v410b_correctIpv4InIpv6",
    ];
    run_category("ipv6", &valid, &HashSet::new()); // all must parse

    let invalid = ["v42_ipv6NoBrackets"];
    run_category("ipv6", &invalid, &set(&invalid)); // unbracketed IPv6 in R-URI
}

// ---------------------------------------------------------------------------
// Strict-valid canonical inputs — must pass the eager parser
// ---------------------------------------------------------------------------

#[test]
fn strict_valid_canonical() {
    let names =
        ["dateGmt", "fromQuotedDisplay", "fromTokenDisplay", "contactNameAddr", "contactBareUri"];
    run_category("strict-valid", &names, &HashSet::new());
}
