//! What ONE message must state on its own — the obligations an endpoint
//! discharges by writing the message correctly, with no other message involved.
//! ELEVEN of them, every one charging the endpoint that EMITTED the message:
//!
//!   - [`BranchPrefix`] (§8.1.1.7) — a request's top Via states a `branch`
//!     beginning with the magic cookie `z9hG4bK`.
//!   - [`MaxForwards`] (§8.1.1.6) — a request states a Max-Forwards, in range
//!     and no higher than the recommended initial 70.
//!   - [`ContentLength`] (§20.14) — a declared body length is the body's byte
//!     count.
//!   - [`ContentType`] (§7.4.1) — a message carrying a body names its format.
//!   - [`ContactPresence`] (§8.1.1.8) — a dialog-establishing request states the
//!     target later in-dialog requests go to.
//!   - [`NoContactOnBye`] (§15.1) — a BYE states none: it ends the dialog.
//!   - [`ToTagPresence`] (§8.2.6.2) — a response above 100 carries a To-tag.
//!   - [`NoRecordRouteFromUa`] (§16.6) — a UA inserts no Record-Route.
//!   - [`CancelCseqMethod`] (§9.1) — a CANCEL's CSeq method token is `CANCEL`.
//!   - [`No100relRequireOnNonInvite`] (RFC 3262 §4) — only an INVITE may
//!     `Require: 100rel`.
//!   - [`Reliable1xxHeaders`] (RFC 3262 §3) — a 1xx's reliability markers add
//!     up: no reliable 100, and an `RSeq` in range behind every `100rel`.
//!
//! The last two are RFC 3262's and still belong here rather than with the PRACK
//! family: they read the response's own rows and nothing of the RSeq/PRACK walk
//! [`super::prack`] owns.
//!
//! **The occasion is one FRESH message the endpoint emitted.** A retransmission
//! is the same act again, never a second offence, so it opens nothing; the
//! endpoint is charged once for what it wrote.
//!
//! **Every fact is read off the message's own bytes** — `Msg::head` through
//! `sip_message::sniff`, never parsed here. A vantage that carried no header
//! block settles none of these: where the missing bytes decide the VERDICT the
//! occasion stands `Undecidable`, and where they decide whether the obligation
//! arises at all (did this message carry a body? is this a response above 100?)
//! there is no occasion to state.

use sip_message::sniff;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::Obligation;

/// The RFC 3261 §8.1.1.7 magic cookie every `branch` begins with.
const MAGIC_COOKIE: &str = "z9hG4bK";

/// The start-line half an occasion keys on: the request method, or the response
/// status as decimal.
fn on_label(msg: &Msg) -> String {
    match &msg.kind {
        Kind::Request { method } => method.clone(),
        Kind::Response { status } => status.to_string(),
    }
}

/// A finding on `msg` for `rule`, charging the endpoint that emitted it.
fn charge(rule: RuleId, msg: &Msg, mi: usize, decision: Decision) -> Finding {
    Finding {
        rule,
        emitter: msg.src.clone(),
        taker: msg.dst.clone(),
        cseq: msg.cseq,
        relayed: false,
        anchor: mi,
        decision,
    }
}

/// **§8.1.1.7 — the top Via `branch` begins with the magic cookie.** An
/// RFC-3261 client transaction is NAMED by its branch, and the cookie is what
/// tells a downstream element the value is one: a branch without it is read as
/// an RFC 2543 legacy id and matched by the old (From/To/Call-ID/CSeq/URI)
/// rules instead, so responses and CANCELs land on the wrong transaction.
///
/// The occasion is one fresh REQUEST the endpoint sent — responses copy the
/// request's Via stack verbatim and mint nothing. Charges the sender.
pub struct BranchPrefix;

impl Obligation for BranchPrefix {
    fn id(&self) -> RuleId {
        RuleId::BranchPrefix
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !matches!(msg.kind, Kind::Request { .. }) {
                continue;
            }
            let finding = |d| charge(RuleId::BranchPrefix, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            match sniff::via_branch(head) {
                Some(branch) if branch.starts_with(MAGIC_COOKIE) => {
                    out.push(finding(Decision::Compliant))
                }
                Some(branch) => {
                    out.push(finding(Decision::Violated(Evidence::HeaderValueRejected {
                        rejected_msg: mi,
                        rejected_hop: msg.hop,
                        rejected_ts_us: msg.at_us,
                        on: on_label(msg),
                        header: "Via;branch".to_string(),
                        value: branch,
                        expected: format!("a branch beginning with \"{MAGIC_COOKIE}\""),
                    })))
                }
                None => out.push(finding(Decision::Violated(Evidence::RequiredHeaderAbsent {
                    absent_msg: mi,
                    absent_hop: msg.hop,
                    absent_ts_us: msg.at_us,
                    on: on_label(msg),
                    header: "Via;branch".to_string(),
                }))),
            }
        }
        out
    }
}

/// **§8.1.1.6 — a request states a Max-Forwards.** The header caps the hop
/// count so a routing loop self-terminates. A request without one leaves a
/// downstream element nothing to decrement; a value no reader accepts, or one
/// above 255, is malformed; and a value above the recommended initial 70 was
/// MINTED at that count rather than reached by the decrements that legitimately
/// lower it.
///
/// The occasion is one fresh request the endpoint sent. Charges the sender,
/// which is the party that writes the header.
pub struct MaxForwards;

impl Obligation for MaxForwards {
    fn id(&self) -> RuleId {
        RuleId::MaxForwards
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !matches!(msg.kind, Kind::Request { .. }) {
                continue;
            }
            let finding = |d| charge(RuleId::MaxForwards, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let rejected = |value: String, expected: &str| {
                Decision::Violated(Evidence::HeaderValueRejected {
                    rejected_msg: mi,
                    rejected_hop: msg.hop,
                    rejected_ts_us: msg.at_us,
                    on: on_label(msg),
                    header: "Max-Forwards".to_string(),
                    value,
                    expected: expected.to_string(),
                })
            };
            let Some(raw) = sniff::header_value(head, "Max-Forwards") else {
                out.push(finding(Decision::Violated(Evidence::RequiredHeaderAbsent {
                    absent_msg: mi,
                    absent_hop: msg.hop,
                    absent_ts_us: msg.at_us,
                    on: on_label(msg),
                    header: "Max-Forwards".to_string(),
                })));
                continue;
            };
            match raw.trim().parse::<u64>() {
                Ok(v) if v <= 70 => out.push(finding(Decision::Compliant)),
                Ok(v) if v <= 255 => out.push(finding(rejected(v.to_string(), "at most 70"))),
                _ => out.push(finding(rejected(raw, "an integer in 0..=255"))),
            }
        }
        out
    }
}

/// **§20.14 — the declared length is the body's byte count.** A declared length
/// that disagrees with the bytes desyncs a framed stream or truncates a body for
/// a strict peer. The count is in BYTES: a binary body (a `multipart/mixed`
/// part, an in-dialog INFO payload) is measured as it rides the wire, never as
/// decoded text.
///
/// **The occasion is one fresh message the endpoint sent that DECLARES a
/// length** — a message declaring none makes no claim there is anything to
/// check. The verdict needs the whole datagram: a vantage that recorded the head
/// alone already sliced the body to the declared length, so it can only agree
/// with itself, and that occasion is `Undecidable`.
///
/// Charges the sender, which writes header and body together.
pub struct ContentLength;

impl Obligation for ContentLength {
    fn id(&self) -> RuleId {
        RuleId::ContentLength
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(head) = msg.head.as_deref() else { continue };
            let Some(declared) = sniff::content_length(head) else { continue };
            let finding = |d| charge(RuleId::ContentLength, msg, mi, d);
            let Some(carried) = sniff::body(head) else {
                out.push(finding(Decision::Undecidable("the head is unterminated")));
                continue;
            };
            // A vantage that split head from body carries no bytes to count
            // here, and the body it did carry was already cut to the declared
            // length: nothing about the wire's own framing is provable.
            if carried.is_empty() && msg.body.as_deref().is_some_and(|b| !b.is_empty()) {
                out.push(finding(Decision::Undecidable(
                    "this vantage carries head and body apart",
                )));
                continue;
            }
            if declared == carried.len() as u64 {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::HeaderValueRejected {
                rejected_msg: mi,
                rejected_hop: msg.hop,
                rejected_ts_us: msg.at_us,
                on: on_label(msg),
                header: "Content-Length".to_string(),
                value: declared.to_string(),
                expected: carried.len().to_string(),
            })));
        }
        out
    }
}

/// **§7.4.1 — a body names its format.** Without a Content-Type the peer cannot
/// tell an SDP description from a DTMF payload from an ISUP blob, so the body is
/// undeliverable to any handler.
///
/// **The occasion is one fresh message the endpoint sent that CARRIES a body**,
/// read off the header block alone (§20.14 states the length, and a Content-Type
/// on a message with no declared length is itself the claim). A vantage with no
/// header block cannot say whether a body rode at all, so it opens no occasion.
///
/// Charges the sender.
pub struct ContentType;

impl Obligation for ContentType {
    fn id(&self) -> RuleId {
        RuleId::ContentType
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(head) = msg.head.as_deref() else { continue };
            if !sniff::has_body(head) {
                continue;
            }
            let decision = if sniff::has_header(head, "Content-Type") {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::RequiredHeaderAbsent {
                    absent_msg: mi,
                    absent_hop: msg.hop,
                    absent_ts_us: msg.at_us,
                    on: on_label(msg),
                    header: "Content-Type".to_string(),
                })
            };
            out.push(charge(RuleId::ContentType, msg, mi, decision));
        }
        out
    }
}

/// **§8.1.1.8 — a dialog-establishing request states its Contact.** The Contact
/// is the dialog's remote target: without one the peer has no address to send
/// the dialog's later requests to, and the dialog is unusable the moment it is
/// confirmed.
///
/// The occasion is one fresh INVITE or SUBSCRIBE the endpoint sent. Charges the
/// sender.
pub struct ContactPresence;

impl Obligation for ContactPresence {
    fn id(&self) -> RuleId {
        RuleId::ContactPresence
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !(msg.is_request("INVITE") || msg.is_request("SUBSCRIBE")) {
                continue;
            }
            let finding = |d| charge(RuleId::ContactPresence, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            if sniff::has_header(head, "Contact") {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::RequiredHeaderAbsent {
                absent_msg: mi,
                absent_hop: msg.hop,
                absent_ts_us: msg.at_us,
                on: on_label(msg),
                header: "Contact".to_string(),
            })));
        }
        out
    }
}

/// **§15.1 — a BYE states no Contact.** BYE ends the dialog, so a target
/// refresh has nothing left to refresh; a Contact on one is a builder pasting
/// the header onto every method it emits.
///
/// The occasion is one fresh BYE the endpoint sent. Charges the sender.
pub struct NoContactOnBye;

impl Obligation for NoContactOnBye {
    fn id(&self) -> RuleId {
        RuleId::NoContactOnBye
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !msg.is_request("BYE") {
                continue;
            }
            let finding = |d| charge(RuleId::NoContactOnBye, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let contacts = sniff::header_values(head, "Contact");
            let Some(first) = contacts.first() else {
                out.push(finding(Decision::Compliant));
                continue;
            };
            out.push(finding(Decision::Violated(Evidence::ForbiddenHeaderPresent {
                forbidden_msg: mi,
                forbidden_hop: msg.hop,
                forbidden_ts_us: msg.at_us,
                on: on_label(msg),
                header: "Contact".to_string(),
                value: first.clone(),
            })));
        }
        out
    }
}

/// **§8.2.6.2 — a UAS tags every response above 100.** The To-tag is the UAS's
/// half of the §12 dialog identifier; a 180 or a 200 or a 4xx without one cannot
/// be dialog-matched by the UAC, and only `100 Trying` — which names no dialog —
/// is exempt.
///
/// The occasion is one fresh response above 100 the endpoint sent, read off the
/// raw To row: this is precisely the message a strict reader refuses, so the
/// bytes are the only place the defect is visible. A vantage with no header
/// block opens no occasion — it cannot even say the status.
///
/// Charges the responding UAS, which mints the tag.
pub struct ToTagPresence;

impl Obligation for ToTagPresence {
    fn id(&self) -> RuleId {
        RuleId::ToTagPresence
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(head) = msg.head.as_deref() else { continue };
            let Some(status) = sniff::resp_status(head) else { continue };
            if status <= 100 {
                continue;
            }
            let decision = if sniff::to_tag(head).is_empty() {
                Decision::Violated(Evidence::RequiredHeaderAbsent {
                    absent_msg: mi,
                    absent_hop: msg.hop,
                    absent_ts_us: msg.at_us,
                    on: status.to_string(),
                    header: "To;tag".to_string(),
                })
            } else {
                Decision::Compliant
            };
            out.push(charge(RuleId::ToTagPresence, msg, mi, decision));
        }
        out
    }
}

/// **§16.6 — Record-Route is a proxy mechanism.** A UA does not stay in the
/// route set: inserting Record-Route puts it into every later in-dialog request
/// the peer builds, under a URI only that UA understands. A B2BUA is a UA, and
/// the rows it inserts are recognisable by the leg markers it stamps into the
/// URI.
///
/// The occasion is one fresh request the endpoint sent. Only a row carrying a
/// B2BUA leg marker (`callRef=` / `leg=`) is judged — a genuine proxy's own
/// Record-Route is its right, and consumer policy narrows the rule to lanes that
/// declared themselves proxies. Charges the endpoint that inserted the row.
pub struct NoRecordRouteFromUa;

impl Obligation for NoRecordRouteFromUa {
    fn id(&self) -> RuleId {
        RuleId::NoRecordRouteFromUa
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !matches!(msg.kind, Kind::Request { .. }) {
                continue;
            }
            let finding = |d| charge(RuleId::NoRecordRouteFromUa, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let marked = sniff::header_values(head, "Record-Route")
                .into_iter()
                .find(|rr| rr.contains("callRef=") || rr.contains("leg="));
            let Some(rr) = marked else {
                out.push(finding(Decision::Compliant));
                continue;
            };
            out.push(finding(Decision::Violated(Evidence::ForbiddenHeaderPresent {
                forbidden_msg: mi,
                forbidden_hop: msg.hop,
                forbidden_ts_us: msg.at_us,
                on: on_label(msg),
                header: "Record-Route".to_string(),
                value: rr,
            })));
        }
        out
    }
}

/// **§9.1 — a CANCEL's CSeq method token is `CANCEL`.** The CANCEL reuses the
/// CSeq NUMBER of the request it cancels but carries its own method token; a
/// token naming the cancelled method makes the CANCEL a malformed copy of it.
///
/// The occasion is one fresh CANCEL the endpoint sent, and it is always
/// DECIDED: both tokens are wire-model facts, so no vantage can fail to settle
/// it. Defence in depth — every parser on the path already refuses a wire
/// CANCEL whose request method and CSeq method disagree, so only a message
/// built past field extraction can reach here. Charges the sender.
pub struct CancelCseqMethod;

impl Obligation for CancelCseqMethod {
    fn id(&self) -> RuleId {
        RuleId::CancelCseqMethod
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat || !msg.is_request("CANCEL") {
                continue;
            }
            let decision = if msg.cseq_method.eq_ignore_ascii_case("CANCEL") {
                Decision::Compliant
            } else {
                Decision::Violated(Evidence::HeaderValueRejected {
                    rejected_msg: mi,
                    rejected_hop: msg.hop,
                    rejected_ts_us: msg.at_us,
                    on: on_label(msg),
                    header: "CSeq;method".to_string(),
                    value: msg.cseq_method.clone(),
                    expected: "CANCEL".to_string(),
                })
            };
            out.push(charge(RuleId::CancelCseqMethod, msg, mi, decision));
        }
        out
    }
}

/// **RFC 3262 §4 — only an INVITE may `Require: 100rel`.** "A Require header
/// with the value 100rel MUST NOT be present in any requests excepting INVITE."
/// The reliable-provisional contract belongs to the INVITE transaction PRACK
/// acknowledges; a BYE or OPTIONS demanding it draws a 420 Bad Extension from
/// any strict peer.
///
/// The occasion is one fresh NON-INVITE request the endpoint sent. `Require`
/// alone is judged — `Proxy-Require` states a hop-by-hop demand §4 does not
/// name. Charges the sender that stamped the tag.
pub struct No100relRequireOnNonInvite;

impl Obligation for No100relRequireOnNonInvite {
    fn id(&self) -> RuleId {
        RuleId::No100relRequireOnNonInvite
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat
                || !matches!(msg.kind, Kind::Request { .. })
                || msg.is_request("INVITE")
            {
                continue;
            }
            let finding = |d| charge(RuleId::No100relRequireOnNonInvite, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            if !sniff::require_has_100rel(head) {
                out.push(finding(Decision::Compliant));
                continue;
            }
            out.push(finding(Decision::Violated(Evidence::ForbiddenHeaderPresent {
                forbidden_msg: mi,
                forbidden_hop: msg.hop,
                forbidden_ts_us: msg.at_us,
                on: on_label(msg),
                header: "Require".to_string(),
                value: "100rel".to_string(),
            })));
        }
        out
    }
}

/// The largest `RSeq` RFC 3262 §3 admits: the value space is `1..=2^31-1`.
const RSEQ_MAX: u64 = 2_147_483_647;

/// **RFC 3262 §3 — a 1xx's reliability markers add up.** A UAS MUST NOT try to
/// send 100 (Trying) reliably, so a 100 carries neither `RSeq` nor
/// `Require: 100rel`; and a 1xx that DOES claim `100rel` carries the `RSeq` the
/// UAC's PRACK will `RAck`, in `1..=2^31-1`. A missing or out-of-range value
/// leaves the PRACK nothing to name, and the transaction stalls on a
/// provisional that can never be acknowledged.
///
/// **The occasion is one fresh 1xx the endpoint sent, and it draws ONE
/// finding** however many rows are at fault — writing the reliability contract
/// wrongly is a single act. Presence and readability are asked separately: a
/// row no reader accepts is itself out of range, and saying so is not the same
/// as saying the header is absent. Charges the responding UAS.
pub struct Reliable1xxHeaders;

impl Obligation for Reliable1xxHeaders {
    fn id(&self) -> RuleId {
        RuleId::Reliable1xxHeaders
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Some(status) = msg.status().filter(|s| (100..200).contains(s)) else { continue };
            let finding = |d| charge(RuleId::Reliable1xxHeaders, msg, mi, d);
            let Some(head) = msg.head.as_deref() else {
                out.push(finding(Decision::Undecidable("no header block at this vantage")));
                continue;
            };
            let rseq_row = sniff::header_value(head, "RSeq");
            let has_100rel = sniff::require_has_100rel(head);
            let violated = |carried: Vec<String>, admits: &str| {
                Decision::Violated(Evidence::Reliable1xx {
                    reliable_1xx_msg: mi,
                    reliable_1xx_hop: msg.hop,
                    reliable_1xx_ts_us: msg.at_us,
                    status,
                    carried,
                    admits: admits.to_string(),
                })
            };

            if status == 100 {
                let mut carried = Vec::new();
                if let Some(raw) = &rseq_row {
                    carried.push(format!("RSeq: {}", raw.trim()));
                }
                if has_100rel {
                    carried.push("Require: 100rel".to_string());
                }
                out.push(finding(if carried.is_empty() {
                    Decision::Compliant
                } else {
                    violated(carried, "a 100 (Trying) carrying neither — it is never reliable")
                }));
                continue;
            }

            // 101-199: `Require: 100rel` is what makes a provisional reliable.
            if !has_100rel {
                out.push(finding(Decision::Compliant));
                continue;
            }
            let Some(raw) = rseq_row else {
                out.push(finding(violated(
                    vec!["Require: 100rel".to_string()],
                    "an RSeq naming the provisional the PRACK will acknowledge",
                )));
                continue;
            };
            let raw = raw.trim().to_string();
            out.push(finding(match sniff::rseq_of(head) {
                Some(rseq) if (1..=RSEQ_MAX).contains(&rseq) => Decision::Compliant,
                _ => violated(
                    vec![format!("RSeq: {raw}"), "Require: 100rel".to_string()],
                    "an RSeq in [1, 2^31-1]",
                ),
            }));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    //! The family's OWN semantics: which message is an occasion, what the bytes
    //! settle, and what a vantage carrying less than the datagram can say.

    use std::collections::BTreeMap;

    use crate::verdict::{Decision, Evidence, Finding};
    use crate::wire::{Kind, Msg, Observation, WireView};

    use super::super::Obligation;
    use super::*;

    const UA: &str = "127.0.0.1:5060";
    const PEER: &str = "127.0.0.1:5070";

    /// A request the UA SENT, with `extra` header rows and `body` bytes; the
    /// Content-Length row is written by the caller through `extra`.
    fn request(method: &str, extra: &str, body: &[u8]) -> Msg {
        let mut text = format!(
            "{method} sip:bob@127.0.0.1:5070 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: <sip:bob@h>\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 {method}\r\n"
        );
        if !extra.is_empty() {
            text.push_str(extra);
            text.push_str("\r\n");
        }
        text.push_str("\r\n");
        let mut head = text.into_bytes();
        head.extend_from_slice(body);
        Msg {
            at_us: 1_000,
            src: UA.to_string(),
            dst: PEER.to_string(),
            hop: 0,
            repeat: false,
            kind: Kind::Request { method: method.to_string() },
            call_id: "c1".to_string(),
            cseq: 1,
            cseq_method: method.to_string(),
            via_branch: Some("z9hG4bK-1".to_string()),
            from_tag: Some("at".to_string()),
            to_tag: None,
            head: Some(head),
            body: Some(body.to_vec()),
        }
    }

    /// A response the UA SENT, `to_tag` omitted where `None`.
    fn response(status: u16, to_tag: Option<&str>) -> Msg {
        let to = match to_tag {
            Some(t) => format!("<sip:bob@h>;tag={t}"),
            None => "<sip:bob@h>".to_string(),
        };
        let head = format!(
            "SIP/2.0 {status} Ringing\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: {to}\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        );
        let mut m = request("INVITE", "Content-Length: 0", b"");
        m.kind = Kind::Response { status };
        m.to_tag = to_tag.map(str::to_string);
        m.head = Some(head.into_bytes());
        m.body = Some(Vec::new());
        m
    }

    fn obs(msgs: &[Msg]) -> Observation {
        let mut endpoint_last_us: BTreeMap<String, u64> = BTreeMap::new();
        let mut last_us = 0;
        for m in msgs {
            last_us = last_us.max(m.at_us);
            for ep in [&m.src, &m.dst] {
                let at = endpoint_last_us.entry(ep.clone()).or_default();
                *at = (*at).max(m.at_us);
            }
        }
        Observation { last_us, endpoint_last_us, closed: true }
    }

    fn run(rule: &dyn Obligation, msgs: &[Msg]) -> Vec<Finding> {
        rule.eval(&WireView { msgs, obs: &obs(msgs) })
    }

    /// The only occasion of the whole family is a message the endpoint EMITTED
    /// fresh: a retransmission is the same act again.
    #[test]
    fn a_retransmission_opens_no_occasion() {
        let mut again = request("INVITE", "Content-Length: 0", b"");
        again.repeat = true;
        assert!(run(&BranchPrefix, &[again]).is_empty());
    }

    // ── branch-prefix ───────────────────────────────────────────────────────

    #[test]
    fn a_cookie_prefixed_branch_is_compliant() {
        let f = run(&BranchPrefix, &[request("INVITE", "Content-Length: 0", b"")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert_eq!(f[0].emitter, UA, "the sender is charged");
    }

    #[test]
    fn a_legacy_branch_is_violated() {
        let mut m = request("INVITE", "Content-Length: 0", b"");
        m.head = Some(
            String::from_utf8(m.head.take().unwrap())
                .unwrap()
                .replace("z9hG4bK-1", "legacy-2543")
                .into_bytes(),
        );
        let f = run(&BranchPrefix, &[m]);
        let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) =
            &f[0].decision
        else {
            panic!("branch evidence: {:?}", f[0].decision)
        };
        assert_eq!(value, "legacy-2543");
        assert!(expected.contains("z9hG4bK"), "{expected}");
    }

    #[test]
    fn a_branchless_request_is_violated_as_an_absence() {
        let mut m = request("INVITE", "Content-Length: 0", b"");
        m.head = Some(
            String::from_utf8(m.head.take().unwrap())
                .unwrap()
                .replace(";branch=z9hG4bK-1", "")
                .into_bytes(),
        );
        let f = run(&BranchPrefix, &[m]);
        assert!(
            matches!(
                &f[0].decision,
                Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                    if header == "Via;branch"
            ),
            "{:?}",
            f[0].decision
        );
    }

    /// A vantage with no bytes cannot say what the request stated: the occasion
    /// stands, UNDECIDED.
    #[test]
    fn a_request_without_header_bytes_is_undecidable() {
        let mut m = request("INVITE", "Content-Length: 0", b"");
        m.head = None;
        let f = run(&BranchPrefix, &[m]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(!f[0].decided(), "{:?}", f[0].decision);
    }

    // ── max-forwards ────────────────────────────────────────────────────────

    #[test]
    fn max_forwards_at_seventy_is_compliant() {
        let f = run(&MaxForwards, &[request("INVITE", "Max-Forwards: 70\r\nContent-Length: 0", b"")]);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn a_request_without_max_forwards_is_violated() {
        let f = run(&MaxForwards, &[request("INVITE", "Content-Length: 0", b"")]);
        assert!(
            matches!(
                &f[0].decision,
                Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                    if header == "Max-Forwards"
            ),
            "{:?}",
            f[0].decision
        );
    }

    /// Above 70 the count was minted, not decremented.
    #[test]
    fn max_forwards_above_seventy_is_violated() {
        let f =
            run(&MaxForwards, &[request("INVITE", "Max-Forwards: 200\r\nContent-Length: 0", b"")]);
        let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) =
            &f[0].decision
        else {
            panic!("max-forwards evidence: {:?}", f[0].decision)
        };
        assert_eq!((value.as_str(), expected.as_str()), ("200", "at most 70"));
    }

    #[test]
    fn an_unreadable_max_forwards_is_violated() {
        let f =
            run(&MaxForwards, &[request("INVITE", "Max-Forwards: many\r\nContent-Length: 0", b"")]);
        let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) =
            &f[0].decision
        else {
            panic!("max-forwards evidence: {:?}", f[0].decision)
        };
        assert_eq!(value, "many");
        assert!(expected.contains("0..=255"), "{expected}");
    }

    // ── content-length ──────────────────────────────────────────────────────

    #[test]
    fn a_matching_content_length_is_compliant() {
        let f = run(&ContentLength, &[request("INVITE", "Content-Length: 5", b"v=0\r\n")]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn a_length_above_the_bytes_is_violated() {
        let f = run(&ContentLength, &[request("INVITE", "Content-Length: 99", b"v=0\r\n")]);
        let Decision::Violated(Evidence::HeaderValueRejected { value, expected, .. }) =
            &f[0].decision
        else {
            panic!("content-length evidence: {:?}", f[0].decision)
        };
        assert_eq!((value.as_str(), expected.as_str()), ("99", "5"));
    }

    /// The count is RAW BYTES: a non-UTF-8 body measured as decoded text would
    /// inflate every byte into a replacement character and flag a faithful
    /// message.
    #[test]
    fn a_binary_body_is_counted_in_bytes() {
        let body: [u8; 8] = [0xFF, 0xFE, 0x00, 0x80, 0xC0, 0x01, 0x02, 0x03];
        let f = run(&ContentLength, &[request("INFO", "Content-Length: 8", &body)]);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);

        let f = run(&ContentLength, &[request("INFO", "Content-Length: 5", &body)]);
        let Decision::Violated(Evidence::HeaderValueRejected { expected, .. }) = &f[0].decision
        else {
            panic!("content-length evidence: {:?}", f[0].decision)
        };
        assert_eq!(expected, "8", "the raw byte count, not the decoded one");
    }

    /// A message declaring no length claims nothing to check.
    #[test]
    fn a_message_declaring_no_length_is_no_occasion() {
        assert!(run(&ContentLength, &[request("INVITE", "Subject: x", b"")]).is_empty());
    }

    /// A vantage that kept the head and the body apart already cut the body to
    /// the declared length, so it can only agree with itself.
    #[test]
    fn a_split_head_and_body_vantage_is_undecidable() {
        let mut m = request("INVITE", "Content-Length: 5", b"");
        m.body = Some(b"v=0\r\n".to_vec());
        let f = run(&ContentLength, &[m]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(!f[0].decided(), "{:?}", f[0].decision);
    }

    // ── content-type ────────────────────────────────────────────────────────

    #[test]
    fn a_body_with_content_type_is_compliant() {
        let f = run(
            &ContentType,
            &[request("INVITE", "Content-Type: application/sdp\r\nContent-Length: 5", b"v=0\r\n")],
        );
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn a_body_without_content_type_is_violated() {
        let f = run(&ContentType, &[request("INVITE", "Content-Length: 5", b"v=0\r\n")]);
        assert!(
            matches!(
                &f[0].decision,
                Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                    if header == "Content-Type"
            ),
            "{:?}",
            f[0].decision
        );
    }

    /// A message carrying no body owes no Content-Type — no obligation arises.
    #[test]
    fn a_bodiless_message_is_no_occasion() {
        assert!(run(&ContentType, &[request("INVITE", "Content-Length: 0", b"")]).is_empty());
    }

    // ── contact-presence / no-contact-on-bye ────────────────────────────────

    #[test]
    fn an_invite_with_contact_is_compliant_and_one_without_is_violated() {
        let with = request("INVITE", "Contact: <sip:a@h>\r\nContent-Length: 0", b"");
        assert!(matches!(run(&ContactPresence, &[with])[0].decision, Decision::Compliant));
        let f = run(&ContactPresence, &[request("INVITE", "Content-Length: 0", b"")]);
        assert!(
            matches!(
                &f[0].decision,
                Decision::Violated(Evidence::RequiredHeaderAbsent { header, .. })
                    if header == "Contact"
            ),
            "{:?}",
            f[0].decision
        );
    }

    /// Only a dialog-ESTABLISHING method owes a Contact.
    #[test]
    fn a_bye_is_no_contact_presence_occasion() {
        assert!(run(&ContactPresence, &[request("BYE", "Content-Length: 0", b"")]).is_empty());
    }

    #[test]
    fn a_bye_carrying_contact_is_violated() {
        let f = run(&NoContactOnBye, &[request("BYE", "Contact: <sip:a@h>\r\nContent-Length: 0", b"")]);
        let Decision::Violated(Evidence::ForbiddenHeaderPresent { header, value, .. }) =
            &f[0].decision
        else {
            panic!("bye-contact evidence: {:?}", f[0].decision)
        };
        assert_eq!((header.as_str(), value.as_str()), ("Contact", "<sip:a@h>"));
        let clean = run(&NoContactOnBye, &[request("BYE", "Content-Length: 0", b"")]);
        assert!(matches!(clean[0].decision, Decision::Compliant), "{:?}", clean[0].decision);
    }

    // ── to-tag-presence ─────────────────────────────────────────────────────

    #[test]
    fn a_tagged_response_above_100_is_compliant() {
        let f = run(&ToTagPresence, &[response(180, Some("bt"))]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    #[test]
    fn an_untagged_response_above_100_is_violated() {
        let f = run(&ToTagPresence, &[response(180, None)]);
        assert!(
            matches!(
                &f[0].decision,
                Decision::Violated(Evidence::RequiredHeaderAbsent { header, on, .. })
                    if header == "To;tag" && on == "180"
            ),
            "{:?}",
            f[0].decision
        );
    }

    /// `100 Trying` names no dialog, so it owes no tag — and a request is no
    /// occasion at all.
    #[test]
    fn a_100_and_a_request_are_no_occasion() {
        assert!(run(&ToTagPresence, &[response(100, None)]).is_empty());
        assert!(run(&ToTagPresence, &[request("INVITE", "Content-Length: 0", b"")]).is_empty());
    }

    // ── no-record-route-from-ua ─────────────────────────────────────────────

    #[test]
    fn a_marked_record_route_is_violated() {
        let f = run(
            &NoRecordRouteFromUa,
            &[request("INVITE", "Record-Route: <sip:b2b@h;callRef=7>\r\nContent-Length: 0", b"")],
        );
        let Decision::Violated(Evidence::ForbiddenHeaderPresent { header, value, .. }) =
            &f[0].decision
        else {
            panic!("record-route evidence: {:?}", f[0].decision)
        };
        assert_eq!(header, "Record-Route");
        assert!(value.contains("callRef=7"), "{value}");
    }

    /// A proxy's own Record-Route carries no B2BUA leg marker and is its right.
    #[test]
    fn an_unmarked_record_route_is_compliant() {
        let f = run(
            &NoRecordRouteFromUa,
            &[request("INVITE", "Record-Route: <sip:p@h;lr>\r\nContent-Length: 0", b"")],
        );
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    // ── cancel-cseq-method ──────────────────────────────────────────────────

    /// A CANCEL whose CSeq token names the cancelled method, built past field
    /// extraction — the only way the state is reachable at all.
    #[test]
    fn a_cancel_whose_cseq_names_another_method_is_violated() {
        let mut m = request("CANCEL", "Content-Length: 0", b"");
        m.cseq_method = "INVITE".to_string();
        let f = run(&CancelCseqMethod, &[m]);
        let Decision::Violated(Evidence::HeaderValueRejected { header, value, expected, .. }) =
            &f[0].decision
        else {
            panic!("cancel-cseq evidence: {:?}", f[0].decision)
        };
        assert_eq!((header.as_str(), value.as_str(), expected.as_str()), ("CSeq;method", "INVITE", "CANCEL"));
    }

    /// Both tokens are wire-model facts, so a byte-less vantage still decides.
    #[test]
    fn a_well_formed_cancel_is_compliant_even_without_bytes() {
        let mut m = request("CANCEL", "Content-Length: 0", b"");
        m.head = None;
        let f = run(&CancelCseqMethod, &[m]);
        assert_eq!(f.len(), 1, "{f:?}");
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert!(run(&CancelCseqMethod, &[request("BYE", "Content-Length: 0", b"")]).is_empty());
    }

    // ── no-100rel-require-on-non-invite ─────────────────────────────────────

    #[test]
    fn a_non_invite_requiring_100rel_is_violated_and_an_invite_is_no_occasion() {
        let f = run(
            &No100relRequireOnNonInvite,
            &[request("BYE", "Require: 100rel\r\nContent-Length: 0", b"")],
        );
        let Decision::Violated(Evidence::ForbiddenHeaderPresent { on, header, value, .. }) =
            &f[0].decision
        else {
            panic!("100rel evidence: {:?}", f[0].decision)
        };
        assert_eq!((on.as_str(), header.as_str(), value.as_str()), ("BYE", "Require", "100rel"));

        let invite = request("INVITE", "Require: 100rel\r\nContent-Length: 0", b"");
        assert!(run(&No100relRequireOnNonInvite, &[invite]).is_empty());
        let plain = run(&No100relRequireOnNonInvite, &[request("BYE", "Content-Length: 0", b"")]);
        assert!(matches!(plain[0].decision, Decision::Compliant), "{:?}", plain[0].decision);
    }

    /// `Proxy-Require` states a hop-by-hop demand §4 does not name.
    #[test]
    fn a_proxy_require_100rel_is_not_this_rules_offence() {
        let f = run(
            &No100relRequireOnNonInvite,
            &[request("BYE", "Proxy-Require: 100rel\r\nContent-Length: 0", b"")],
        );
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
    }

    // ── reliable-1xx-headers ────────────────────────────────────────────────

    /// A 1xx the UAS sent, with `extra` rows spelled by the caller.
    fn provisional(status: u16, extra: &str) -> Msg {
        let head = format!(
            "SIP/2.0 {status} X\r\n\
             Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK-1\r\n\
             From: <sip:alice@h>;tag=at\r\n\
             To: <sip:bob@h>;tag=bt\r\n\
             Call-ID: c1\r\n\
             CSeq: 1 INVITE\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
        );
        let mut m = response(status, Some("bt"));
        m.head = Some(head.into_bytes());
        m
    }

    #[test]
    fn a_plain_100_and_a_reliable_183_with_a_good_rseq_are_compliant() {
        for m in [provisional(100, ""), provisional(183, "Require: 100rel\r\nRSeq: 1\r\n")] {
            let f = run(&Reliable1xxHeaders, &[m]);
            assert_eq!(f.len(), 1, "{f:?}");
            assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        }
    }

    /// A 100 made reliable by BOTH markers is ONE finding naming both.
    #[test]
    fn a_reliable_100_is_one_finding_naming_every_marker() {
        let f = run(&Reliable1xxHeaders, &[provisional(100, "RSeq: 1\r\nRequire: 100rel\r\n")]);
        assert_eq!(f.len(), 1, "one act, one finding: {f:?}");
        let Decision::Violated(Evidence::Reliable1xx { status, carried, .. }) = &f[0].decision
        else {
            panic!("reliable-1xx evidence: {:?}", f[0].decision)
        };
        assert_eq!(*status, 100);
        assert_eq!(carried.as_slice(), ["RSeq: 1", "Require: 100rel"]);
    }

    #[test]
    fn a_reliable_provisional_without_an_rseq_is_violated() {
        let f = run(&Reliable1xxHeaders, &[provisional(183, "Require: 100rel\r\n")]);
        let Decision::Violated(Evidence::Reliable1xx { carried, admits, .. }) = &f[0].decision
        else {
            panic!("reliable-1xx evidence: {:?}", f[0].decision)
        };
        assert_eq!(carried.as_slice(), ["Require: 100rel"]);
        assert!(admits.contains("RSeq"), "{admits}");
    }

    /// Zero, and a row no reader accepts, are both out of the `1..=2^31-1`
    /// space — presence and readability asked separately.
    #[test]
    fn an_out_of_range_or_unreadable_rseq_is_violated() {
        for extra in ["Require: 100rel\r\nRSeq: 0\r\n", "Require: 100rel\r\nRSeq: many\r\n"] {
            let f = run(&Reliable1xxHeaders, &[provisional(183, extra)]);
            let Decision::Violated(Evidence::Reliable1xx { admits, .. }) = &f[0].decision else {
                panic!("reliable-1xx evidence: {:?}", f[0].decision)
            };
            assert!(admits.contains("2^31-1"), "{admits}");
        }
    }

    /// An UNreliable 18x owes no RSeq, and a final is no occasion at all.
    #[test]
    fn a_plain_180_is_compliant_and_a_final_is_no_occasion() {
        let f = run(&Reliable1xxHeaders, &[provisional(180, "")]);
        assert!(matches!(f[0].decision, Decision::Compliant), "{:?}", f[0].decision);
        assert!(run(&Reliable1xxHeaders, &[response(200, Some("bt"))]).is_empty());
    }
}
