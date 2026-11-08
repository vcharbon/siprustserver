//! Must accept, generated: values of every header's grammar and of each start
//! line, each in a whole message through the production path, then through its
//! typed reader.

use sip_message::header;

use super::abnf::{Grammar, Rng};
use super::message::{raw, read, typed, ReadCheck, Template};
use super::parameters::{repeats_a_name, Layout};
use super::ratchet::{settle_rows, Failures};
use super::stated::failing_row;

/// Values per target in the default lane, a test of well under a second.
const VALUES_PER_TARGET: usize = 120;

/// Values per target the exemption mode reads under its parser.
const VALUES_PER_TARGET_UNDER_ANOTHER_PARSER: usize = 30;

/// Values per target in the slow lane.
const VALUES_PER_TARGET_SLOW: usize = 5000;

/// The suite's grammar: RFC 3261 §25.1 and the extensions' grammars, errata
/// applied.
pub fn grammar() -> Grammar {
    Grammar::parse(include_str!("grammar.abnf"))
}

/// Where a generated line goes.
#[derive(Clone, Copy)]
enum Place {
    /// In place of the template's line of this name, or after them.
    Line(&'static str),
    /// Below the template's top Via.
    LowerVia,
    /// The start line.
    Request,
    /// The start line of a response.
    Response,
}

/// Which template holds the line.
#[derive(Clone, Copy)]
enum Base {
    Request(&'static str),
    Response(u16, &'static str),
}

struct Target {
    row: &'static str,
    rule: &'static str,
    place: Place,
    base: Base,
    read: ReadCheck,
}

const fn target(
    row: &'static str,
    rule: &'static str,
    place: Place,
    base: Base,
    read: ReadCheck,
) -> Target {
    Target { row, rule, place, base, read }
}

const OPTIONS: Base = Base::Request("OPTIONS");

macro_rules! typed_target {
    ($row:literal, $rule:literal, $key:literal, $base:expr, $ty:ty) => {
        target($row, $rule, Place::Line($key), $base, typed::<$ty>)
    };
}

macro_rules! raw_target {
    ($row:literal, $rule:literal, $key:literal, $name:literal) => {
        target($row, $rule, Place::Line($key), OPTIONS, |m| raw(m, $name))
    };
}

/// Every header of the crate's coverage table, and each start line.
fn targets() -> Vec<Target> {
    vec![
        target("request-line", "Request-Line", Place::Request, OPTIONS, |_| Ok(())),
        target(
            "status-line",
            "Status-Line",
            Place::Response,
            Base::Response(200, "OPTIONS"),
            |_| Ok(()),
        ),
        typed_target!("via-top", "Via-top", "via", OPTIONS, header::Via),
        target("via-lower", "Via", Place::LowerVia, OPTIONS, typed::<header::Via>),
        typed_target!("from", "From", "from", OPTIONS, header::From),
        typed_target!("to", "To", "to", OPTIONS, header::To),
        typed_target!("call-id", "Call-ID", "call-id", OPTIONS, header::CallId),
        typed_target!("cseq", "CSeq", "cseq", OPTIONS, header::CSeq),
        typed_target!("contact", "Contact", "contact", Base::Request("REGISTER"), header::Contact),
        typed_target!("route", "Route", "route", OPTIONS, header::RouteEntry),
        typed_target!("record-route", "Record-Route", "rr", OPTIONS, header::RecordRouteEntry),
        typed_target!("path", "Path", "path", OPTIONS, header::PathEntry),
        typed_target!("service-route", "Service-Route", "sr", OPTIONS, header::ServiceRouteEntry),
        typed_target!("max-forwards", "Max-Forwards", "max-forwards", OPTIONS, header::MaxForwards),
        typed_target!("content-length", "Content-Length", "cl", OPTIONS, header::ContentLength),
        typed_target!("expires", "Expires", "expires", OPTIONS, header::Expires),
        typed_target!("min-expires", "Min-Expires", "min-expires", OPTIONS, header::MinExpires),
        typed_target!("session-expires", "Session-Expires", "se", OPTIONS, header::SessionExpires),
        typed_target!("min-se", "Min-SE", "min-se", OPTIONS, header::MinSe),
        typed_target!("rseq", "RSeq", "rseq", Base::Response(183, "INVITE"), header::RSeq),
        typed_target!("rack", "RAck", "rack", Base::Request("PRACK"), header::RAck),
        typed_target!("require", "Require", "require", OPTIONS, header::Require),
        typed_target!("proxy-require", "Proxy-Require", "pr", OPTIONS, header::ProxyRequire),
        typed_target!("unsupported", "Unsupported", "unsup", OPTIONS, header::Unsupported),
        typed_target!("supported", "Supported", "supported", OPTIONS, header::Supported),
        typed_target!("allow", "Allow", "allow", OPTIONS, header::Allow),
        typed_target!("allow-events", "Allow-Events", "ae", OPTIONS, header::AllowEvents),
        typed_target!("privacy", "Privacy", "privacy", OPTIONS, header::Privacy),
        typed_target!("event", "Event", "event", Base::Request("SUBSCRIBE"), header::Event),
        typed_target!(
            "subscription-state",
            "Subscription-State",
            "subscription-state",
            Base::Request("NOTIFY"),
            header::SubscriptionState
        ),
        typed_target!("refer-to", "Refer-To", "refer-to", Base::Request("REFER"), header::ReferTo),
        typed_target!(
            "referred-by",
            "Referred-By",
            "rb",
            Base::Request("REFER"),
            header::ReferredBy
        ),
        typed_target!("refer-sub", "Refer-Sub", "rs", Base::Request("REFER"), header::ReferSub),
        typed_target!(
            "replaces",
            "Replaces",
            "replaces",
            Base::Request("INVITE"),
            header::Replaces
        ),
        typed_target!("reply-to", "Reply-To", "reply-to", OPTIONS, header::ReplyTo),
        typed_target!("reason", "Reason", "reason", Base::Request("BYE"), header::Reason),
        typed_target!(
            "retry-after",
            "Retry-After",
            "ra",
            Base::Response(503, "OPTIONS"),
            header::RetryAfter
        ),
        typed_target!("content-type", "Content-Type", "ct", OPTIONS, header::MediaType),
        typed_target!(
            "content-disposition",
            "Content-Disposition",
            "cd",
            OPTIONS,
            header::ContentDisposition
        ),
        typed_target!("accept", "Accept", "accept", OPTIONS, header::AcceptRange),
        typed_target!("authorization", "Authorization", "authz", OPTIONS, header::Authorization),
        typed_target!(
            "proxy-authorization",
            "Proxy-Authorization",
            "pauthz",
            OPTIONS,
            header::ProxyAuthorization
        ),
        typed_target!(
            "www-authenticate",
            "WWW-Authenticate",
            "wa",
            Base::Response(401, "OPTIONS"),
            header::WwwAuthenticate
        ),
        typed_target!(
            "proxy-authenticate",
            "Proxy-Authenticate",
            "pa",
            Base::Response(407, "OPTIONS"),
            header::ProxyAuthenticate
        ),
        typed_target!("pai", "P-Asserted-Identity", "pai", OPTIONS, header::PAssertedIdentity),
        typed_target!("ppi", "P-Preferred-Identity", "ppi", OPTIONS, header::PPreferredIdentity),
        typed_target!("diversion", "Diversion", "diversion", OPTIONS, header::Diversion),
        typed_target!("history-info", "History-Info", "hi", OPTIONS, header::HistoryInfo),
        typed_target!("remote-party-id", "Remote-Party-ID", "rpid", OPTIONS, header::RemotePartyId),
        typed_target!("geolocation", "Geolocation", "geo", OPTIONS, header::Geolocation),
        typed_target!(
            "geolocation-error",
            "Geolocation-Error",
            "geo-e",
            OPTIONS,
            header::GeolocationError
        ),
        typed_target!(
            "geolocation-routing",
            "Geolocation-Routing",
            "geo-r",
            OPTIONS,
            header::GeolocationRouting
        ),
        typed_target!(
            "charging-vector",
            "P-Charging-Vector",
            "cv",
            OPTIONS,
            header::ChargingVector
        ),
        raw_target!("subject", "Subject", "subject", "Subject"),
        raw_target!("warning", "Warning", "warning", "Warning"),
        raw_target!("alert-info", "Alert-Info", "alert-info", "Alert-Info"),
        raw_target!("call-info", "Call-Info", "call-info", "Call-Info"),
        raw_target!("error-info", "Error-Info", "error-info", "Error-Info"),
        raw_target!("authentication-info", "Authentication-Info", "ai", "Authentication-Info"),
        raw_target!("organization", "Organization", "organization", "Organization"),
        raw_target!("priority", "Priority", "priority", "Priority"),
        raw_target!("date", "Date", "date", "Date"),
        raw_target!("timestamp", "Timestamp", "timestamp", "Timestamp"),
        raw_target!("mime-version", "MIME-Version", "mime", "MIME-Version"),
        raw_target!("server", "Server", "server", "Server"),
        raw_target!("user-agent", "User-Agent", "ua", "User-Agent"),
        raw_target!("accept-encoding", "Accept-Encoding", "ae2", "Accept-Encoding"),
        raw_target!("accept-language", "Accept-Language", "al", "Accept-Language"),
        raw_target!("content-encoding", "Content-Encoding", "ce", "Content-Encoding"),
        raw_target!("content-language", "Content-Language", "cl2", "Content-Language"),
        raw_target!("in-reply-to", "In-Reply-To", "irt", "In-Reply-To"),
        raw_target!("p-early-media", "P-Early-Media", "pem", "P-Early-Media"),
        raw_target!(
            "p-access-network-info",
            "P-Access-Network-Info",
            "pani",
            "P-Access-Network-Info"
        ),
        raw_target!("resource-priority", "Resource-Priority", "rp", "Resource-Priority"),
        raw_target!("user-to-user", "User-to-User", "uui", "User-to-User"),
        raw_target!("flow-timer", "Flow-Timer", "ft", "Flow-Timer"),
        target("extension", "extension-header", Place::Line("x"), OPTIONS, |_| Ok(())),
    ]
}

/// The whole message holding `generated`.
fn wrap(target: &Target, generated: &[u8]) -> Vec<u8> {
    let base = match target.base {
        Base::Request(method) => Template::request(method),
        Base::Response(status, method) => Template::response(status, method),
    };
    let text = String::from_utf8_lossy(generated);
    let placed = match target.place {
        Place::Line("cseq") => {
            let method = text.split_ascii_whitespace().last().unwrap_or_default();
            base.start_line(format!("{method} sip:bob@example.com SIP/2.0")).with("cseq", generated)
        }
        Place::Line("rack") => {
            base.with("to", "To: <sip:bob@example.com>;tag=b1").with("rack", generated)
        }
        Place::Line("cl") => {
            let digits = text.rsplit(':').next().unwrap_or_default().trim();
            let length: usize = digits.parse().unwrap_or(0);
            base.unframed()
                .with("ct", "Content-Type: text/plain")
                .with("cl", generated)
                .body(vec![b'x'; length])
        }
        Place::Line(name) => base.with(name, generated),
        Place::LowerVia => base.with("via2", generated),
        Place::Request => {
            let method = text.split(' ').next().unwrap_or_default();
            base.with("cseq", format!("CSeq: 1 {method}")).start_line(generated)
        }
        Place::Response => base.start_line(generated),
    };
    placed.bytes()
}

/// How `target`'s header lays out its parameters.
fn layout(target: &Target) -> Layout {
    let comma_separated = matches!(
        target.row,
        "authorization"
            | "proxy-authorization"
            | "www-authenticate"
            | "proxy-authenticate"
            | "authentication-info"
    );
    let comments = matches!(target.row, "retry-after" | "server" | "user-agent");
    Layout { comma_separated, comments }
}

/// One value of `target`'s rule that names no parameter twice (§7.3.1,
/// §19.1.1): a repeat is a row of its own, never a generated value.
fn value(grammar: &Grammar, target: &Target, rng: &mut Rng) -> Vec<u8> {
    let layout = layout(target);
    for _ in 0..1000 {
        let generated = grammar.generate(target.rule, rng);
        if !repeats_a_name(&generated, layout) {
            return generated;
        }
    }
    panic!("{}: no value without a repeated parameter name", target.row)
}

/// `count` values of every target through the production path. A target whose
/// values are mostly refused for a stated reason exercises nothing else, so it
/// fails: its grammar is narrowed instead.
fn sweep(count: usize) -> Failures {
    let grammar = grammar();
    let mut failures = Failures::default();
    for target in targets() {
        let mut rng = Rng::named(target.row);
        let mut explained = 0;
        for _ in 0..count {
            let generated = value(&grammar, &target, &mut rng);
            let outcome = match read(&wrap(&target, &generated)) {
                Ok(msg) => (target.read)(&msg),
                Err(e) => Err(e.reason),
            };
            let Err(reason) = outcome else { continue };
            match failing_row(&reason) {
                Some(row) => {
                    let input = String::from_utf8_lossy(&generated);
                    failures.note(row, target.row, &input, &reason);
                }
                None => explained += 1,
            }
        }
        assert!(explained * 4 <= count, "{}: {explained}/{count} refused", target.row);
    }
    failures
}

/// One value of the target `pick` names, its choices steered by `steer`, in a
/// whole message: the production path reads it and its typed reader reads its
/// line, unless a stated rule refuses the form ([`failing_row`] is `None`).
pub fn must_accept(grammar: &Grammar, pick: u8, steer: &[u8]) -> Result<(), String> {
    let all = targets();
    let target = &all[usize::from(pick) % all.len()];
    let generated = value(grammar, target, &mut Rng::steered(target.row, steer));
    let outcome = match read(&wrap(target, &generated)) {
        Ok(msg) => (target.read)(&msg),
        Err(e) => Err(e.reason),
    };
    match outcome {
        Err(reason) if failing_row(&reason).is_some() => {
            let input = String::from_utf8_lossy(&generated);
            Err(format!("{}: {input:?} does not read: {reason}", target.row))
        }
        _ => Ok(()),
    }
}

/// How many targets [`must_accept`] picks among.
pub fn target_count() -> usize {
    targets().len()
}

/// A few values of every target: a sweep under another parser.
pub fn datagrams() -> Vec<Vec<u8>> {
    let grammar = grammar();
    let mut all = Vec::new();
    for target in targets() {
        let mut rng = Rng::named(target.row);
        for _ in 0..VALUES_PER_TARGET_UNDER_ANOTHER_PARSER {
            all.push(wrap(&target, &value(&grammar, &target, &mut rng)));
        }
    }
    all
}

/// The must-accept property a fuzzer drives, over steering bytes that take the
/// first alternative, the last, and a spread of both.
#[test]
fn a_steered_value_reads() {
    let grammar = grammar();
    let steers: [Vec<u8>; 3] = [vec![0; 64], vec![255; 64], (0..=255).collect()];
    for pick in 0..target_count() {
        let pick = u8::try_from(pick).expect("fewer than 256 targets");
        for steer in &steers {
            must_accept(&grammar, pick, steer).unwrap_or_else(|e| panic!("{e}"));
        }
    }
}

#[test]
fn every_generated_value_reads() {
    settle_rows("abnf", &sweep(VALUES_PER_TARGET));
}

/// The long sweep holds its own listing, `abnf-long/`: a row only its volume
/// reaches is listed there and leaves the default lane alone.
#[test]
#[ignore = "slow lane: real clock >= 1 s"]
fn every_generated_value_of_a_long_sweep_reads() {
    settle_rows("abnf-long", &sweep(VALUES_PER_TARGET_SLOW));
}
