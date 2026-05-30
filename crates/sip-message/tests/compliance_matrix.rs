//! RFC 4475 torture + CVE-regression + RFC 3261 param-gap + RFC 5118 IPv6
//! compliance matrix for the ported custom parser. Port of
//! `tests/sip/parser-compliance.test.ts` (the `custom` column).
//!
//! Fixtures are the byte-exact files under `tests/fixtures/<category>/`,
//! dumped verbatim from the TS source (the `sipMsg` template results) so the
//! wire bytes match 1:1.
//!
//! The TS matrix tests valid/CVE/param-gap/IPv6 with eager `parse` only, and
//! invalid/strict-valid with `parseStrict` (eager + `runAllStrictLazyParsers`).
//! We mirror that split: [`verdict_eager`] vs [`verdict_strict`] (the latter
//! runs `SipMessage::validate_strict()`), selected per category.

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

/// Eager parse only — the TS `impl.parse(buf)`.
fn verdict_eager(bytes: &[u8]) -> Result<(), String> {
    CustomParser::new().parse(bytes).map(|_| ()).map_err(|e| e.reason)
}

/// Eager parse + strict pass — the TS `parseStrict` (rejection at either step
/// counts). The strict pass intentionally over-rejects some RFC-valid torture
/// display names, so the TS applies it ONLY to the invalid + strict-valid
/// corpora — never the full valid torture set.
fn verdict_strict(bytes: &[u8]) -> Result<(), String> {
    let msg = CustomParser::new().parse(bytes).map_err(|e| e.reason)?;
    msg.validate_strict().map_err(|e| e.reason)
}

/// Run a category: assert each fixture's accept/reject matches expectation,
/// collecting every mismatch so one run surfaces them all.
fn run_category(category: &str, names: &[&str], expect_reject: &HashSet<&str>, strict: bool) {
    let mut mismatches: Vec<String> = Vec::new();
    for &name in names {
        let bytes = fixture(category, name);
        let v = if strict { verdict_strict(&bytes) } else { verdict_eager(&bytes) };
        let should_reject = expect_reject.contains(name);
        match (&v, should_reject) {
            (Ok(()), true) => mismatches.push(format!("  {category}/{name}: expected REJECT, but ACCEPTED")),
            (Err(reason), false) => {
                mismatches.push(format!("  {category}/{name}: expected ACCEPT, but REJECTED ({reason})"))
            }
            _ => {}
        }
    }
    assert!(mismatches.is_empty(), "compliance mismatches in {category}:\n{}", mismatches.join("\n"));
}

fn set(items: &[&'static str]) -> HashSet<&'static str> {
    items.iter().copied().collect()
}

// ---------------------------------------------------------------------------
// RFC 4475 §3.1.1 — valid messages (must parse, except ADR-0007 rejections)
// ---------------------------------------------------------------------------

#[test]
fn rfc4475_valid() {
    let names = [
        "shortTorturousInvite",   // 3.1.1.1
        "wideRangeValidChars",    // 3.1.1.2
        "validPercentEscaping",   // 3.1.1.3
        "escapedNulls",           // 3.1.1.4
        "percentNotEscape",       // 3.1.1.5
        "noLwsBeforeAngleBracket",// 3.1.1.6
        "longValues",             // 3.1.1.7
        "extraTrailingOctets",    // 3.1.1.8
        "semicolonInUserPart",    // 3.1.1.9
        "variedTransports",       // 3.1.1.10
        "multipartMime",          // 3.1.1.11
        "unusualReasonPhrase",    // 3.1.1.12
        "emptyReasonPhrase",      // 3.1.1.13
    ];
    // ADR-0007: custom rejects 3.1.1.1 (top-Via no magic cookie), 3.1.1.7
    // (long-values top Via magic-cookie-less), 3.1.1.10 (UNKNOWN transport).
    let reject = set(&["shortTorturousInvite", "longValues", "variedTransports"]);
    run_category("rfc4475-valid", &names, &reject, false);
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
    // Reject every invalid fixture EXCEPT the two the TS `custom` column is
    // also lenient on — start-line *shape* malformations that sit outside both
    // the eager strict-header pipeline and the strict re-parsers:
    //   - trailingSpacesInRequestLine (3.1.2.10)
    //   - escapedHeadersInUri (3.1.2.11)
    // The Date / bare-addr-spec / non-token-display cases (3.1.2.12/.13/.15)
    // are now caught by `validate_strict()` (the ported strict re-parsers).
    let lenient = set(&["trailingSpacesInRequestLine", "escapedHeadersInUri"]);
    let reject: HashSet<&str> = names.iter().copied().filter(|n| !lenient.contains(n)).collect();
    run_category("rfc4475-invalid", &names, &reject, true);
}

// ---------------------------------------------------------------------------
// CVE regression (must reject all)
// ---------------------------------------------------------------------------

#[test]
fn cve_regression() {
    let names = ["cve_2023_27598", "cve_2023_27599", "cve_2023_28098"];
    let reject = set(&names);
    run_category("cve", &names, &reject, false);
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
    run_category("param-gaps", &names, &reject, false);
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
    run_category("ipv6", &valid, &HashSet::new(), false); // all must parse

    let invalid = ["v42_ipv6NoBrackets"];
    run_category("ipv6", &invalid, &set(&invalid), false); // unbracketed IPv6 in R-URI
}

// ---------------------------------------------------------------------------
// Strict-valid canonical inputs — must pass the eager parser
// ---------------------------------------------------------------------------

#[test]
fn strict_valid_canonical() {
    let names = ["dateGmt", "fromQuotedDisplay", "fromTokenDisplay", "contactNameAddr", "contactBareUri"];
    run_category("strict-valid", &names, &HashSet::new(), true);
}

// ---------------------------------------------------------------------------
// rvoip parity oracle (ADR-0001) — second matrix column, feature-gated.
// rvoip is the looser lexical reference; ADR-0007 adds gates on top, so the
// expected relation is custom-accept ⊆ rvoip-accept (custom is stricter).
// ---------------------------------------------------------------------------
#[cfg(feature = "rvoip-oracle")]
mod rvoip_parity {
    use super::*;
    use sip_message::parser::rvoip::RvoipParser;

    fn all_fixtures() -> Vec<(String, String, Vec<u8>)> {
        let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        root.push("tests/fixtures");
        let mut out = Vec::new();
        for cat in fs::read_dir(&root).unwrap() {
            let cat = cat.unwrap().path();
            if !cat.is_dir() { continue; }
            let catname = cat.file_name().unwrap().to_string_lossy().to_string();
            for f in fs::read_dir(&cat).unwrap() {
                let p = f.unwrap().path();
                if p.extension().map(|e| e == "sip").unwrap_or(false) {
                    let name = p.file_stem().unwrap().to_string_lossy().to_string();
                    out.push((catname.clone(), name, fs::read(&p).unwrap()));
                }
            }
        }
        out.sort();
        out
    }

    /// rvoip's expected REJECT set, empirically snapshotted at rvoip-sip-core
    /// 0.1.26. Every other fixture under tests/fixtures/ rvoip ACCEPTS. The set
    /// diverges from custom's in both directions (ADR-0001): rvoip accepts most
    /// ADR-0007 rejections (it is the looser reference), yet its strict mode is
    /// stricter than custom on four RFC 4475 torture-valid messages
    /// (emptyReasonPhrase / extraTrailingOctets / multipartMime /
    /// wideRangeValidChars). Pinning the snapshot catches behavioural drift on
    /// an rvoip upgrade.
    const RVOIP_REJECTS: &[&str] = &[
        "cve/cve_2023_27598",
        "rfc4475-invalid/contentLengthTooLarge",
        "rfc4475-invalid/embeddedLwsInUri",
        "rfc4475-invalid/negativeContentLength",
        "rfc4475-invalid/overlargeResponseCode",
        "rfc4475-valid/emptyReasonPhrase",
        "rfc4475-valid/extraTrailingOctets",
        "rfc4475-valid/multipartMime",
        "rfc4475-valid/wideRangeValidChars",
    ];

    /// The rvoip column: assert rvoip's accept/reject verdict on every fixture
    /// matches the pinned snapshot.
    #[test]
    fn rvoip_verdicts_match_snapshot() {
        let rvoip = RvoipParser;
        let expect_reject: HashSet<&str> = RVOIP_REJECTS.iter().copied().collect();
        let mut mismatches: Vec<String> = Vec::new();
        for (cat, name, bytes) in all_fixtures() {
            let key = format!("{cat}/{name}");
            let rejected = rvoip.parse(&bytes).is_err();
            let should_reject = expect_reject.contains(key.as_str());
            if rejected != should_reject {
                mismatches.push(format!(
                    "  {key}: rvoip {} but snapshot expected {}",
                    if rejected { "REJECTED" } else { "ACCEPTED" },
                    if should_reject { "REJECT" } else { "ACCEPT" }
                ));
            }
        }
        assert!(
            mismatches.is_empty(),
            "rvoip verdict drift vs snapshot (rvoip-sip-core upgraded?):\n{}",
            mismatches.join("\n")
        );
    }

    /// Report (not assert) the ADR-0007 additions: fixtures the custom parser
    /// rejects that rvoip accepts. Each is a gate ADR-0007 adds on top of
    /// rvoip's looser grammar; custom is strictly stronger on these.
    #[test]
    fn report_adr_0007_additions() {
        let custom = CustomParser::new();
        let rvoip = RvoipParser;
        let mut count = 0;
        for (cat, name, bytes) in all_fixtures() {
            if custom.parse(&bytes).is_err() && rvoip.parse(&bytes).is_ok() {
                count += 1;
                eprintln!("ADR-0007 gate (custom-only reject): {cat}/{name}");
            }
        }
        eprintln!(
            "rvoip-parity: {count} fixtures rejected by custom but accepted by rvoip \
             (ADR-0007 additions)."
        );
    }
}
