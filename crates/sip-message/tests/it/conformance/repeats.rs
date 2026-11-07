//! Must refuse, a second line: each single-valued header the stack knows appears
//! once (RFC 3261 §7.3.1), compact forms counted, whatever its two values. A list
//! header, the credentials and challenges §7.3.1 names, and a header the stack
//! does not know may repeat.

use super::break_rule::refused;
use super::message::Template;
use super::ratchet::Row;

/// Which template holds a header.
#[derive(Clone, Copy)]
enum Base {
    Request(&'static str),
    Response(u16, &'static str),
}

impl Base {
    fn template(self) -> Template {
        match self {
            Base::Request(method) => Template::request(method),
            Base::Response(status, method) => Template::response(status, method),
        }
    }
}

const OPTIONS: Base = Base::Request("OPTIONS");

/// `(row, name, template line it replaces or "", base, a value, another value)`:
/// the single-valued headers the stack knows, the one list the repeat operator
/// reads.
type Single = (&'static str, &'static str, &'static str, Base, &'static str, &'static str);

const SINGLE_VALUED: &[Single] = &[
    (
        "from",
        "From",
        "from",
        OPTIONS,
        "<sip:alice@example.com>;tag=a1",
        "<sip:carol@example.com>;tag=c1",
    ),
    ("to", "To", "to", OPTIONS, "<sip:bob@example.com>", "<sip:dave@example.com>"),
    (
        "call-id",
        "Call-ID",
        "call-id",
        OPTIONS,
        "conformance-1@192.0.2.10",
        "conformance-3@192.0.2.10",
    ),
    ("cseq", "CSeq", "cseq", OPTIONS, "1 OPTIONS", "2 OPTIONS"),
    ("max-forwards", "Max-Forwards", "max-forwards", OPTIONS, "70", "69"),
    ("content-length", "Content-Length", "", OPTIONS, "0", "00"),
    ("expires", "Expires", "", OPTIONS, "3600", "60"),
    ("min-expires", "Min-Expires", "", Base::Response(423, "REGISTER"), "60", "120"),
    ("session-expires", "Session-Expires", "", Base::Request("INVITE"), "1800;refresher=uac", "90"),
    ("min-se", "Min-SE", "", Base::Request("INVITE"), "90;x=1", "120"),
    ("rseq", "RSeq", "", Base::Response(183, "INVITE"), "987", "988"),
    ("rack", "RAck", "", Base::Request("PRACK"), "776656 1 INVITE", "776657 1 INVITE"),
    ("event", "Event", "", Base::Request("SUBSCRIBE"), "refer;id=1", "presence"),
    (
        "subscription-state",
        "Subscription-State",
        "",
        Base::Request("NOTIFY"),
        "active;expires=60",
        "pending",
    ),
    (
        "refer-to",
        "Refer-To",
        "",
        Base::Request("REFER"),
        "<sip:carol@example.com>;x=1",
        "<sip:dave@example.com>",
    ),
    (
        "referred-by",
        "Referred-By",
        "",
        Base::Request("REFER"),
        "<sip:alice@example.com>",
        "<sip:carol@example.com>",
    ),
    ("refer-sub", "Refer-Sub", "", Base::Request("REFER"), "false", "true"),
    (
        "replaces",
        "Replaces",
        "",
        Base::Request("INVITE"),
        "abc@example.com;to-tag=a;from-tag=b",
        "def@example.com;to-tag=c;from-tag=d",
    ),
    ("reply-to", "Reply-To", "", OPTIONS, "<sip:alice@example.com>", "<sip:carol@example.com>"),
    ("retry-after", "Retry-After", "", Base::Response(503, "OPTIONS"), "120;duration=60", "60"),
    ("content-type", "Content-Type", "", OPTIONS, "application/sdp;charset=utf-8", "text/plain"),
    (
        "content-disposition",
        "Content-Disposition",
        "",
        OPTIONS,
        "session;handling=optional",
        "render",
    ),
    ("content-transfer-encoding", "Content-Transfer-Encoding", "", OPTIONS, "binary", "8bit"),
    ("geolocation-error", "Geolocation-Error", "", Base::Response(424, "INVITE"), "100", "200"),
    ("geolocation-routing", "Geolocation-Routing", "", Base::Request("INVITE"), "no", "yes"),
    (
        "p-charging-vector",
        "P-Charging-Vector",
        "",
        Base::Request("INVITE"),
        "icid-value=a1",
        "icid-value=b2",
    ),
    ("subject", "Subject", "", OPTIONS, "a", "b"),
    ("organization", "Organization", "", OPTIONS, "a", "b"),
    ("priority", "Priority", "", OPTIONS, "normal", "urgent"),
    ("date", "Date", "", OPTIONS, "Sat, 13 Nov 2010 23:29:00 GMT", "Sat, 13 Nov 2010 23:29:01 GMT"),
    ("timestamp", "Timestamp", "", OPTIONS, "54", "55"),
    ("mime-version", "MIME-Version", "", OPTIONS, "1.0", "2.0"),
    ("server", "Server", "", Base::Response(200, "OPTIONS"), "stack/1", "stack/2"),
    ("user-agent", "User-Agent", "", OPTIONS, "stack/1", "stack/2"),
];

/// A long and a compact name of one header, on two lines: one repeat.
const COMPACT_PAIRS: &[(&str, &str, &str, Base)] = &[
    ("content-type", "Content-Type: text/plain", "c: text/plain", OPTIONS),
    ("session-expires", "Session-Expires: 1800", "x: 1800", Base::Request("INVITE")),
    ("event", "Event: refer", "o: refer", Base::Request("SUBSCRIBE")),
    (
        "refer-to",
        "Refer-To: <sip:carol@example.com>",
        "r: <sip:carol@example.com>",
        Base::Request("REFER"),
    ),
    (
        "referred-by",
        "Referred-By: <sip:alice@example.com>",
        "b: <sip:alice@example.com>",
        Base::Request("REFER"),
    ),
    ("subject", "Subject: a", "s: a", OPTIONS),
    ("from", "From: <sip:alice@example.com>;tag=a1", "f: <sip:alice@example.com>;tag=a1", OPTIONS),
    ("to", "To: <sip:bob@example.com>", "t: <sip:bob@example.com>", OPTIONS),
    ("call-id", "Call-ID: conformance-1@192.0.2.10", "i: conformance-1@192.0.2.10", OPTIONS),
    ("content-length", "Content-Length: 0", "l: 0", OPTIONS),
];

/// The two lines in a message of `base`: `first` in place of its own line of
/// `slot`, or both added after its lines.
fn twice(base: Base, slot: &'static str, first: String, second: String) -> Vec<u8> {
    let template = match slot {
        "" if first.starts_with("Content-Length") || first.starts_with("l:") => {
            base.template().unframed().add(first)
        }
        "" => base.template().add(first),
        slot => base.template().with(slot, first),
    };
    template.add(second).bytes()
}

/// A refused second line: one of the fields a response echoes leaves no partial,
/// and a response is never answered.
fn outcome(raw: &[u8], row: &str, base: Base) -> Result<(), String> {
    let echoed = matches!(row, "from" | "to" | "call-id" | "cseq");
    refused(raw, !echoed && matches!(base, Base::Request(_)))
}

/// Every repeat datagram, row named.
fn cases() -> Vec<(String, Vec<u8>, Result<(), String>)> {
    let mut all = Vec::new();
    for &(row, name, slot, base, one, other) in SINGLE_VALUED {
        for (operator, second) in [("same", one), ("different", other)] {
            let raw = twice(base, slot, format!("{name}: {one}"), format!("{name}: {second}"));
            let result = outcome(&raw, row, base);
            all.push((format!("refuse/repeated-header-{operator}/{row}"), raw, result));
        }
    }
    for &(row, long, compact, base) in COMPACT_PAIRS {
        let slot = match row {
            "from" | "to" | "call-id" => row,
            _ => "",
        };
        let raw = twice(base, slot, long.to_string(), compact.to_string());
        let result = outcome(&raw, row, base);
        all.push((format!("refuse/compact-repeat/{row}"), raw, result));
    }
    all
}

/// The rows of every repeat.
pub fn rows() -> Vec<Row> {
    cases().into_iter().map(|(row, _, result)| Row::new(row, result)).collect()
}

/// The datagrams of every repeat, for a sweep under another parser.
pub fn datagrams() -> Vec<Vec<u8>> {
    cases().into_iter().map(|(_, raw, _)| raw).collect()
}
