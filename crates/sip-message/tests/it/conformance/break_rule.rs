//! Must refuse, by construction: a valid line of every header ingress vouches for
//! or acts on, in a whole message, with one operator breaking one named rule. The
//! operator defines the violation, so no grammar is consulted. A refused line
//! names its ADR-0054 outcome: a line a response echoes leaves no partial, any
//! other in a request is answered 400, a response is never answered. A re-cased
//! case-insensitive literal is the one operator that must be accepted.

use sip_message::header;

use super::message::{read, typed, ReadCheck, Template};
use super::ratchet::{settle, Row};
use super::repeats;

/// A header the operators break.
struct Subject {
    row: &'static str,
    name: &'static str,
    /// The template line it replaces, or the name it goes under.
    slot: &'static str,
    /// A valid value, `|` marking a position inside it outside any quoted-string.
    value: &'static str,
    base: fn() -> Template,
    /// Its line is echoed by a response, so a refusal of it leaves no partial.
    echoed: bool,
    /// A parameter the value holds: name, its value, another value.
    param: Option<(&'static str, &'static str, &'static str)>,
    /// What separates parameters: `;`, or `,` in credentials.
    separator: &'static str,
    /// Whether the value holds a bracketed URI.
    uri: bool,
    /// Whether the header's grammar folds a list.
    list: bool,
    /// Its token-only parameter, quoted: `branch` and `tag`, the two a stated rule
    /// makes a token. Any other parameter also reads as `generic-param`, whose
    /// `gen-value` admits a quoted-string, so its quoted form is valid.
    quoted: Option<&'static str>,
    /// Ingress vouches for it (accepted means readable), so ingress refuses a
    /// broken value. Any other header may refuse it at its typed read instead.
    vouched: bool,
    /// Its case-insensitive literals, re-cased.
    recased: Option<&'static str>,
    read: ReadCheck,
}

fn options() -> Template {
    Template::request("OPTIONS")
}

fn register() -> Template {
    Template::request("REGISTER")
}

fn invite() -> Template {
    Template::request("INVITE")
}

fn prack() -> Template {
    Template::request("PRACK")
}

fn refer() -> Template {
    Template::request("REFER")
}

fn subscribe() -> Template {
    Template::request("SUBSCRIBE")
}

fn notify() -> Template {
    Template::request("NOTIFY")
}

fn reliable() -> Template {
    Template::response(183, "INVITE")
}

fn unavailable() -> Template {
    Template::response(503, "OPTIONS")
}

const CREDENTIALS: &str = "Digest username=\"carol\", realm=\"example.com\", |nonce=\"n1\", \
                           uri=\"sip:bob@example.com\", \
                           response=\"0123456789abcdef0123456789abcdef\"";

const CREDENTIALS_RECASED: &str = "DIGEST USERNAME=\"carol\", REALM=\"example.com\", \
                                   NONCE=\"n1\", URI=\"sip:bob@example.com\", \
                                   RESPONSE=\"0123456789abcdef0123456789abcdef\"";

const BASE: Subject = Subject {
    row: "",
    name: "",
    slot: "",
    value: "",
    base: options,
    echoed: false,
    param: None,
    separator: ";",
    uri: false,
    list: false,
    quoted: None,
    vouched: true,
    recased: None,
    read: |_| Ok(()),
};

/// The headers ingress vouches for, and those the stack acts on.
fn subjects() -> Vec<Subject> {
    let mut all = dialog_subjects();
    all.extend(numeric_subjects());
    all.extend(acted_on_subjects());
    all
}

/// The echoed fields, Contact and the route set.
fn dialog_subjects() -> Vec<Subject> {
    vec![
        Subject {
            row: "via",
            name: "Via",
            slot: "via",
            value: "SIP/2.0/UDP |192.0.2.30:5060;branch=z9hG4bK-sub",
            echoed: true,
            param: Some(("branch", "z9hG4bK-sub", "z9hG4bK-two")),
            list: true,
            quoted: Some("SIP/2.0/UDP 192.0.2.30:5060;branch=\"z9hG4bK-sub\""),
            recased: Some("sIp/2.0/uDp 192.0.2.30:5060;BRANCH=z9hG4bK-sub"),
            read: typed::<header::Via>,
            ..BASE
        },
        Subject {
            row: "from",
            name: "From",
            slot: "from",
            value: "<sip:carol@example.com>|;tag=c1",
            echoed: true,
            param: Some(("tag", "c1", "c2")),
            uri: true,
            quoted: Some("<sip:carol@example.com>;tag=\"c1\""),
            recased: Some("<SIP:carol@example.com>;TAG=c1"),
            read: typed::<header::From>,
            ..BASE
        },
        Subject {
            row: "to",
            name: "To",
            slot: "to",
            value: "<sip:dave@example.com>|;tag=d1",
            echoed: true,
            param: Some(("tag", "d1", "d2")),
            uri: true,
            quoted: Some("<sip:dave@example.com>;tag=\"d1\""),
            recased: Some("<SIP:dave@example.com>;TAG=d1"),
            read: typed::<header::To>,
            ..BASE
        },
        Subject {
            row: "call-id",
            name: "Call-ID",
            slot: "call-id",
            value: "conformance|-2@192.0.2.10",
            echoed: true,
            read: typed::<header::CallId>,
            ..BASE
        },
        Subject {
            row: "cseq",
            name: "CSeq",
            slot: "cseq",
            value: "1|0 OPTIONS",
            echoed: true,
            read: typed::<header::CSeq>,
            ..BASE
        },
        Subject {
            row: "contact",
            name: "Contact",
            slot: "contact",
            value: "<sip:carol@192.0.2.30:5060>|;expires=60",
            base: register,
            param: Some(("expires", "60", "120")),
            uri: true,
            list: true,
            recased: Some("<SIP:carol@192.0.2.30:5060>;EXPIRES=60"),
            read: typed::<header::Contact>,
            ..BASE
        },
        Subject {
            row: "route",
            name: "Route",
            slot: "route",
            value: "<sip:proxy.example.com;lr>|;x=1",
            param: Some(("x", "1", "2")),
            uri: true,
            list: true,
            recased: Some("<SIP:proxy.example.com;LR>;X=1"),
            read: typed::<header::RouteEntry>,
            ..BASE
        },
        Subject {
            row: "record-route",
            name: "Record-Route",
            slot: "record-route",
            value: "<sip:proxy.example.com;lr>|;x=1",
            param: Some(("x", "1", "2")),
            uri: true,
            list: true,
            recased: Some("<SIP:proxy.example.com;LR>;X=1"),
            read: typed::<header::RecordRouteEntry>,
            ..BASE
        },
    ]
}

/// The headers of the numeric registry, and their neighbours in the grammar.
fn numeric_subjects() -> Vec<Subject> {
    vec![
        Subject {
            row: "max-forwards",
            name: "Max-Forwards",
            slot: "max-forwards",
            value: "7|0",
            read: typed::<header::MaxForwards>,
            ..BASE
        },
        Subject {
            row: "content-length",
            name: "Content-Length",
            slot: "content-length",
            value: "|0",
            read: typed::<header::ContentLength>,
            ..BASE
        },
        Subject {
            row: "expires",
            name: "Expires",
            slot: "expires",
            value: "36|00",
            read: typed::<header::Expires>,
            ..BASE
        },
        Subject {
            row: "min-expires",
            name: "Min-Expires",
            slot: "min-expires",
            value: "6|0",
            read: typed::<header::MinExpires>,
            ..BASE
        },
        Subject {
            row: "session-expires",
            name: "Session-Expires",
            slot: "session-expires",
            value: "18|00;refresher=uac",
            base: invite,
            param: Some(("refresher", "uac", "uas")),
            recased: Some("1800;REFRESHER=uac"),
            read: typed::<header::SessionExpires>,
            ..BASE
        },
        Subject {
            row: "min-se",
            name: "Min-SE",
            slot: "min-se",
            value: "9|0;x=1",
            base: invite,
            param: Some(("x", "1", "2")),
            read: typed::<header::MinSe>,
            ..BASE
        },
        Subject {
            row: "rseq",
            name: "RSeq",
            slot: "rseq",
            value: "98|7",
            base: reliable,
            read: typed::<header::RSeq>,
            ..BASE
        },
        Subject {
            row: "rack",
            vouched: false,
            name: "RAck",
            slot: "rack",
            value: "776656 |1 INVITE",
            base: prack,
            read: typed::<header::RAck>,
            ..BASE
        },
    ]
}

/// Credentials, the body's description, and the headers the stack acts on.
fn acted_on_subjects() -> Vec<Subject> {
    vec![
        Subject {
            row: "authorization",
            name: "Authorization",
            slot: "authorization",
            value: CREDENTIALS,
            param: Some(("realm", "\"example.com\"", "\"example.org\"")),
            separator: ", ",
            recased: Some(CREDENTIALS_RECASED),
            read: typed::<header::Authorization>,
            ..BASE
        },
        Subject {
            row: "proxy-authorization",
            name: "Proxy-Authorization",
            slot: "proxy-authorization",
            value: CREDENTIALS,
            param: Some(("realm", "\"example.com\"", "\"example.org\"")),
            separator: ", ",
            recased: Some(CREDENTIALS_RECASED),
            read: typed::<header::ProxyAuthorization>,
            ..BASE
        },
        Subject {
            row: "content-type",
            name: "Content-Type",
            slot: "content-type",
            value: "application/sdp|;charset=utf-8",
            param: Some(("charset", "utf-8", "latin1")),
            recased: Some("APPLICATION/SDP;CHARSET=utf-8"),
            read: typed::<header::MediaType>,
            ..BASE
        },
        Subject {
            row: "content-disposition",
            vouched: true,
            name: "Content-Disposition",
            slot: "content-disposition",
            value: "session|;handling=optional",
            param: Some(("handling", "optional", "required")),
            recased: Some("SESSION;HANDLING=optional"),
            read: typed::<header::ContentDisposition>,
            ..BASE
        },
        Subject {
            row: "event",
            vouched: false,
            name: "Event",
            slot: "event",
            value: "refer|;id=1",
            base: subscribe,
            param: Some(("id", "1", "2")),
            recased: Some("refer;ID=1"),
            read: typed::<header::Event>,
            ..BASE
        },
        Subject {
            row: "refer-to",
            vouched: false,
            name: "Refer-To",
            slot: "refer-to",
            value: "<sip:carol@example.com>|;x=1",
            base: refer,
            param: Some(("x", "1", "2")),
            uri: true,
            recased: Some("<SIP:carol@example.com>;X=1"),
            read: typed::<header::ReferTo>,
            ..BASE
        },
        Subject {
            row: "replaces",
            vouched: false,
            name: "Replaces",
            slot: "replaces",
            value: "abc@example.com|;to-tag=a;from-tag=b",
            base: invite,
            param: Some(("to-tag", "a", "c")),
            recased: Some("abc@example.com;TO-TAG=a;FROM-TAG=b"),
            read: typed::<header::Replaces>,
            ..BASE
        },
        Subject {
            row: "subscription-state",
            vouched: false,
            name: "Subscription-State",
            slot: "subscription-state",
            value: "active|;expires=60",
            base: notify,
            param: Some(("expires", "60", "120")),
            recased: Some("ACTIVE;EXPIRES=60"),
            read: typed::<header::SubscriptionState>,
            ..BASE
        },
        Subject {
            row: "retry-after",
            vouched: false,
            name: "Retry-After",
            slot: "retry-after",
            value: "12|0;duration=60",
            base: unavailable,
            param: Some(("duration", "60", "120")),
            recased: Some("120;DURATION=60"),
            read: typed::<header::RetryAfter>,
            ..BASE
        },
    ]
}

/// The list headers the empty-entry operator also breaks, none vouched for.
type IdentityList = (&'static str, &'static str, &'static str, ReadCheck);

const IDENTITY_LISTS: &[IdentityList] = &[
    (
        "p-asserted-identity",
        "P-Asserted-Identity",
        "<sip:carol@example.com>",
        typed::<header::PAssertedIdentity>,
    ),
    (
        "diversion",
        "Diversion",
        "<sip:carol@example.com>;reason=unknown",
        typed::<header::Diversion>,
    ),
    (
        "history-info",
        "History-Info",
        "<sip:carol@example.com>;index=1",
        typed::<header::HistoryInfo>,
    ),
    ("geolocation", "Geolocation", "<cid:carol@example.com>", typed::<header::Geolocation>),
];

impl Subject {
    fn plain(&self) -> String {
        self.value.replace('|', "")
    }

    /// The whole message holding `line` as its line of this header.
    fn message(&self, line: impl Into<Vec<u8>>) -> Template {
        let template = (self.base)();
        if self.row == "content-length" {
            return template.unframed().with(self.slot, line);
        }
        template.with(self.slot, line)
    }

    fn line(&self, value: &[u8]) -> Vec<u8> {
        [self.name.as_bytes(), b": ", value].concat()
    }

    fn with_value(&self, value: &[u8]) -> Vec<u8> {
        self.message(self.line(value)).bytes()
    }

    /// The refusal of `raw` is the one ADR-0054 gives this line.
    fn refused(&self, raw: &[u8]) -> Result<(), String> {
        let is_response = (self.base)().bytes().starts_with(b"SIP/2.0");
        refused(raw, !self.echoed && !is_response, Some(self.name))
    }

    /// `raw` is refused, or, where ingress does not vouch for this header, its
    /// typed reader refuses it.
    fn not_read(&self, raw: &[u8]) -> Result<(), String> {
        if self.vouched {
            return self.refused(raw);
        }
        match read(raw) {
            Ok(msg) => (self.read)(&msg).err().map_or(Err("read".to_string()), |_| Ok(())),
            Err(_) => Ok(()),
        }
    }

    /// A row whose operator breaks the value: the header's reader answers for it.
    fn row(&self, operator: &str, raw: &[u8]) -> Row {
        Row::new(format!("refuse/{operator}/{}", self.row), self.not_read(raw))
    }

    /// A row whose operator breaks the message, not a line: every line still
    /// reads, so a request is answered (ADR-0054 §2) and a response is not.
    fn message_row(&self, operator: &str, raw: &[u8]) -> Row {
        let is_response = (self.base)().bytes().starts_with(b"SIP/2.0");
        let because = Some("blank line|empty line");
        Row::new(format!("refuse/{operator}/{}", self.row), refused(raw, !is_response, because))
    }

    fn at_mid(&self, inserted: &[u8]) -> Vec<u8> {
        let (before, after) = self.value.split_once('|').expect("a position in the value");
        [before.as_bytes(), inserted, after.as_bytes()].concat()
    }

    /// The first URI of the value, holding `params`.
    fn uri_with(&self, params: &str) -> Vec<u8> {
        self.plain().replacen('>', &format!("{params}>"), 1).into_bytes()
    }
}

/// `raw` is refused, answered or left unanswered as `answered` states, for a
/// reason naming one of `because`'s `|`-separated words, case aside: a refusal
/// for another reason passes no row. Every refusal carries a reason; `None`
/// leaves it unread.
pub fn refused(raw: &[u8], answered: bool, because: Option<&str>) -> Result<(), String> {
    let error = match read(raw) {
        Ok(_) => return Err("accepted".to_string()),
        Err(error) => error,
    };
    if let Some(because) = because {
        let reason = error.reason.to_ascii_lowercase();
        if !because.split('|').any(|word| reason.contains(&word.to_ascii_lowercase())) {
            return Err(format!("refused for another reason: {}", error.reason));
        }
    }
    match (answered, error.partial().is_some()) {
        (true, false) => Err(format!("refused with no partial: {}", error.reason)),
        (false, true) => Err(format!("refused and answered: {}", error.reason)),
        _ => Ok(()),
    }
}

/// What answers for an operator's datagram.
#[derive(Clone, Copy)]
enum Judge {
    /// The value: ingress, or the typed reader of a header ingress does not
    /// vouch for.
    Value,
    /// The message: ingress, every line still read.
    Message,
}

/// One datagram an operator builds of a subject.
struct Case {
    operator: String,
    raw: Vec<u8>,
    judge: Judge,
}

fn case(operator: &str, raw: Vec<u8>) -> Case {
    Case { operator: operator.to_string(), raw, judge: Judge::Value }
}

/// Every operator that applies to `subject`.
fn cases(subject: &Subject) -> Vec<Case> {
    let plain = subject.plain();
    let value = |text: String| subject.with_value(text.as_bytes());
    let mut all = vec![case("junk-after-value", value(format!("{plain} garbage")))];
    let separator = subject.separator;
    if let Some((name, same, other)) = subject.param {
        all.push(case("junk-before-parameter", value(format!("{plain} garbage{separator}x=1"))));
        all.push(case("non-token-parameter-name", value(format!("{plain}{separator}a(b=c"))));
        all.push(case("repeat-parameter-same", value(format!("{plain}{separator}{name}={same}"))));
        all.push(case(
            "repeat-parameter-different",
            value(format!("{plain}{separator}{name}={other}")),
        ));
    }
    if subject.uri {
        let same = subject.uri_with(";xp=1;xp=1");
        all.push(case("repeat-uri-parameter-same", subject.with_value(&same)));
        let other = subject.uri_with(";xp=1;xp=2");
        all.push(case("repeat-uri-parameter-different", subject.with_value(&other)));
    }
    if let Some(quoted) = subject.quoted {
        all.push(case("quoted-token-parameter", subject.with_value(quoted.as_bytes())));
    }
    if subject.list {
        all.push(case("empty-entry-inside", value(format!("{plain},,{plain}"))));
        all.push(case("empty-entry-first", value(format!(",{plain}"))));
        all.push(case("empty-entry-last", value(format!("{plain},"))));
    }
    for (byte, inserted) in [("vt", &b"\x0b"[..]), ("ff", b"\x0c"), ("nbsp", b"\xc2\xa0")] {
        let start = [inserted, plain.as_bytes()].concat();
        all.push(case(&format!("{byte}-at-start"), subject.with_value(&start)));
        let end = [plain.as_bytes(), inserted].concat();
        all.push(case(&format!("{byte}-at-end"), subject.with_value(&end)));
        all.push(case(&format!("{byte}-inside"), subject.with_value(&subject.at_mid(inserted))));
    }
    let line = subject.line(plain.as_bytes());
    let last = subject.message(line.clone()).unframed();
    all.push(Case {
        operator: "no-blank-line".to_string(),
        raw: last.bytes_without_blank_line(),
        judge: Judge::Message,
    });
    all
}

/// Every datagram the operators build: no exemption of the suite's generic set
/// lifts one.
pub fn datagrams() -> Vec<Vec<u8>> {
    let mut all: Vec<Vec<u8>> = subjects().iter().flat_map(cases).map(|case| case.raw).collect();
    all.extend(repeats::datagrams());
    all
}

#[test]
fn every_operator_breaks_its_rule() {
    let mut rows = Vec::new();
    for subject in subjects() {
        for case in cases(&subject) {
            rows.push(match case.judge {
                Judge::Value => subject.row(&case.operator, &case.raw),
                Judge::Message => subject.message_row(&case.operator, &case.raw),
            });
        }
    }
    for &(row, name, value, typed_read) in IDENTITY_LISTS {
        for (operator, list) in [
            ("empty-entry-inside", format!("{value},,{value}")),
            ("empty-entry-first", format!(",{value}")),
            ("empty-entry-last", format!("{value},")),
        ] {
            let outcome = match read(&options().add(format!("{name}: {list}")).bytes()) {
                Ok(msg) => typed_read(&msg).err().map_or(Err("read".to_string()), |_| Ok(())),
                Err(_) => Ok(()),
            };
            rows.push(Row::new(format!("refuse/{operator}/{row}"), outcome));
        }
    }
    rows.extend(repeats::rows());
    settle("refuse", &rows);
}

#[test]
fn a_recased_literal_reads() {
    let mut rows = Vec::new();
    for subject in subjects() {
        let upper = subject.name.to_ascii_uppercase();
        let plain = subject.plain();
        let cases = [
            ("name", format!("{upper}: {plain}")),
            ("name-lower", format!("{}: {plain}", subject.name.to_ascii_lowercase())),
        ];
        for (kind, line) in cases {
            let outcome = match read(&subject.message(line).bytes()) {
                Ok(msg) => (subject.read)(&msg),
                Err(e) => Err(e.reason),
            };
            rows.push(Row::new(format!("recase/{kind}/{}", subject.row), outcome));
        }
        if let Some(recased) = subject.recased {
            let outcome = match read(&subject.with_value(recased.as_bytes())) {
                Ok(msg) => (subject.read)(&msg),
                Err(e) => Err(e.reason),
            };
            rows.push(Row::new(format!("recase/value/{}", subject.row), outcome));
        }
    }
    settle("recase", &rows);
}
