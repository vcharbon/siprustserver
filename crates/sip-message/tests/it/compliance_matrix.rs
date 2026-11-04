//! RFC 4475 torture + CVE-regression + RFC 3261 param-gap + RFC 5118 IPv6
//! compliance matrix of the parser.
//!
//! The RFC 4475 fixtures under `tests/fixtures/rfc4475-*/` are the messages of
//! the RFC's Appendix A bit-exact archive, byte for byte:
//! [`rfc4475_fixtures_are_the_rfc_archive`] decodes the archive from
//! `docs/rfc/rfc4475.txt` and compares every fixture with its message. Each
//! expectation below restates what the RFC says of its message.
//!
//! Every category is read by the one default parse: the grammar of RFC 3261
//! §25.1 and the gates of ADR-0001 are all held at parse time.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::path::PathBuf;

use base64::Engine;
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

/// `(fixture, archive message)` of RFC 4475 §3.1.1, in section order.
const RFC4475_VALID: [(&str, &str); 13] = [
    ("shortTorturousInvite", "wsinv"),      // 3.1.1.1
    ("wideRangeValidChars", "intmeth"),     // 3.1.1.2
    ("validPercentEscaping", "esc01"),      // 3.1.1.3
    ("escapedNulls", "escnull"),            // 3.1.1.4
    ("percentNotEscape", "esc02"),          // 3.1.1.5
    ("noLwsBeforeAngleBracket", "lwsdisp"), // 3.1.1.6
    ("longValues", "longreq"),              // 3.1.1.7
    ("extraTrailingOctets", "dblreq"),      // 3.1.1.8
    ("semicolonInUserPart", "semiuri"),     // 3.1.1.9
    ("variedTransports", "transports"),     // 3.1.1.10
    ("multipartMime", "mpart01"),           // 3.1.1.11
    ("unusualReasonPhrase", "unreason"),    // 3.1.1.12
    ("emptyReasonPhrase", "noreason"),      // 3.1.1.13
];

#[test]
fn rfc4475_valid() {
    let names: Vec<&str> = RFC4475_VALID.iter().map(|(name, _)| *name).collect();
    // 3.1.1.1 and 3.1.1.7 carry a top Via without the magic cookie: RFC 2543
    // requests, refused by scope choice (ADR-0001 gates, departing from RFC
    // 3261 §17.2.3).
    let reject = set(&["shortTorturousInvite", "longValues"]);
    run_category("rfc4475-valid", &names, &reject);
}

// ---------------------------------------------------------------------------
// RFC 4475 §3.1.2 — invalid messages
// ---------------------------------------------------------------------------

/// `(fixture, archive message)` of RFC 4475 §3.1.2, in section order.
const RFC4475_INVALID: [(&str, &str); 19] = [
    ("extraneousSeparators", "badinv01"),    // 3.1.2.1
    ("contentLengthTooLarge", "clerr"),      // 3.1.2.2
    ("negativeContentLength", "ncl"),        // 3.1.2.3
    ("overlargRequestScalars", "scalar02"),  // 3.1.2.4
    ("overlargResponseScalars", "scalarlg"), // 3.1.2.5
    ("unterminatedQuotedString", "quotbal"), // 3.1.2.6
    ("angleBracketRequestUri", "ltgtruri"),  // 3.1.2.7
    ("embeddedLwsInUri", "lwsruri"),         // 3.1.2.8
    ("multipleSPInRequestLine", "lwsstart"), // 3.1.2.9
    ("trailingSpacesInRequestLine", "trws"), // 3.1.2.10
    ("escapedHeadersInUri", "escruri"),      // 3.1.2.11
    ("invalidTimezone", "baddate"),          // 3.1.2.12
    ("unencloseNameAddr", "regbadct"),       // 3.1.2.13
    ("spacesInAddrSpec", "badaspec"),        // 3.1.2.14
    ("nonTokenDisplayName", "baddn"),        // 3.1.2.15
    ("unknownProtocolVersion", "badvers"),   // 3.1.2.16
    ("methodMismatch", "mismatch01"),        // 3.1.2.17
    ("unknownMethodMismatch", "mismatch02"), // 3.1.2.18
    ("overlargeResponseCode", "bigcode"),    // 3.1.2.19
];

#[test]
fn rfc4475_invalid() {
    let names: Vec<&str> = RFC4475_INVALID.iter().map(|(name, _)| *name).collect();
    // Every invalid message is refused but invalidTimezone (3.1.2.12), whose
    // Date the RFC lets an element ignore when it does not use it: a
    // malformed Date never refuses a message (ADR-0053). The trailing SP of
    // 3.1.2.10 is one the RFC lets an element refuse, and it is refused.
    let lenient = set(&["invalidTimezone"]);
    let reject: HashSet<&str> = names.iter().copied().filter(|n| !lenient.contains(n)).collect();
    run_category("rfc4475-invalid", &names, &reject);
}

// ---------------------------------------------------------------------------
// RFC 4475 §3.2 – §3.4 — transaction, application and RFC 2543 messages
// ---------------------------------------------------------------------------

/// `(fixture, archive message)` of RFC 4475 §3.2 to §3.4, in section order.
const RFC4475_SEMANTICS: [(&str, &str); 17] = [
    ("missingTransactionId", "badbranch"),      // 3.2.1
    ("missingRequiredHeaders", "insuf"),        // 3.3.1
    ("unknownSchemeRequestUri", "unkscm"),      // 3.3.2
    ("atypicalSchemeRequestUri", "novelsc"),    // 3.3.3
    ("unknownSchemesInHeaders", "unksm2"),      // 3.3.4
    ("proxyRequireAndRequire", "bext01"),       // 3.3.5
    ("unknownContentType", "invut"),            // 3.3.6
    ("unknownAuthorizationScheme", "regaut01"), // 3.3.7
    ("multipleSingleValueFields", "multi01"),   // 3.3.8
    ("multipleContentLength", "mcl01"),         // 3.3.9
    ("broadcastVia", "bcast"),                  // 3.3.10
    ("maxForwardsZero", "zeromf"),              // 3.3.11
    ("contactHeaderParam", "cparam01"),         // 3.3.12
    ("contactUrlParam", "cparam02"),            // 3.3.13
    ("escapedHeaderInContact", "regescrt"),     // 3.3.14
    ("unacceptableAccept", "sdp01"),            // 3.3.15
    ("rfc2543Syntax", "inv2543"),               // 3.4.1
];

#[test]
fn rfc4475_semantics() {
    let names: Vec<&str> = RFC4475_SEMANTICS.iter().map(|(name, _)| *name).collect();
    // The messages the RFC has an element answer 400 or an error are refused
    // at parse: no Call-ID, From or To (3.3.1), single-value fields repeated
    // (3.3.8), two Content-Length values (3.3.9). 3.2.1's empty transaction
    // identifier is one the RFC lets an element answer 400; the branch reads
    // as the bare magic cookie and is refused. 3.4.1 carries no magic
    // cookie: an RFC 2543 request, refused by scope choice (ADR-0001). Every
    // other message is well formed and parses; what the RFC asks of it
    // (416, 420, 415, 483, …) is the element's answer, not the parser's.
    let reject = set(&[
        "missingTransactionId",
        "missingRequiredHeaders",
        "multipleSingleValueFields",
        "multipleContentLength",
        "rfc2543Syntax",
    ]);
    run_category("rfc4475-semantics", &names, &reject);
}

// ---------------------------------------------------------------------------
// The RFC 4475 fixtures are the RFC's own bytes
// ---------------------------------------------------------------------------

/// The messages of RFC 4475 Appendix A, by archive name: the base64 block
/// between its markers, gunzipped, read as a ustar archive.
fn rfc4475_archive() -> HashMap<String, Vec<u8>> {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("../../docs/rfc/rfc4475.txt");
    let text = fs::read_to_string(&p).unwrap_or_else(|e| panic!("read {p:?}: {e}"));
    // The block runs between the marker lines (the decoding script above them
    // names the markers too, inside a longer line). Appendix A keeps every
    // line of it that is one run of non-space characters: the page footers
    // and headers between the base64 lines hold spaces.
    let base64: String = text
        .lines()
        .map(str::trim)
        .skip_while(|line| *line != "-- BEGIN MESSAGE ARCHIVE --")
        .skip(1)
        .take_while(|line| *line != "-- END MESSAGE ARCHIVE --")
        .filter(|line| !line.is_empty() && !line.contains(char::is_whitespace))
        .collect();
    let gz = base64::engine::general_purpose::STANDARD.decode(base64).expect("base64");
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(gz.as_slice()).read_to_end(&mut tar).expect("gunzip");

    let mut files = HashMap::new();
    let mut at = 0;
    while let Some(header) = tar.get(at..at + 512) {
        if header.iter().all(|&b| b == 0) {
            break;
        }
        let field = |range: std::ops::Range<usize>| {
            let raw = &header[range];
            let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
            String::from_utf8_lossy(&raw[..end]).trim().to_string()
        };
        let name = field(0..100);
        let size = usize::from_str_radix(&field(124..136), 8).expect("octal size");
        let data = tar[at + 512..at + 512 + size].to_vec();
        let name = name.trim_end_matches(".dat").to_string();
        files.insert(name, data);
        at += 512 + size.div_ceil(512) * 512;
    }
    files
}

#[test]
fn rfc4475_fixtures_are_the_rfc_archive() {
    let archive = rfc4475_archive();
    let categories: [(&str, &[(&str, &str)]); 3] = [
        ("rfc4475-valid", &RFC4475_VALID),
        ("rfc4475-invalid", &RFC4475_INVALID),
        ("rfc4475-semantics", &RFC4475_SEMANTICS),
    ];
    let mut differ = Vec::new();
    for (category, fixtures) in categories {
        for (name, message) in fixtures {
            let rfc = archive.get(*message).unwrap_or_else(|| panic!("{message} in the archive"));
            if fixture(category, name) != *rfc {
                differ.push(format!("{category}/{name} ({message})"));
            }
        }
    }
    assert!(differ.is_empty(), "fixtures differing from RFC 4475 Appendix A: {differ:?}");
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
        // RFC 5118 §4.10: the extra colon before an IPv4 tail is tolerated.
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
