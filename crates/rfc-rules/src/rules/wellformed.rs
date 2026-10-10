//! What ONE message must state on its own — the obligations an endpoint
//! discharges by writing the message correctly, with no other message involved.
//! TWELVE of them, every one charging the endpoint that EMITTED the message:
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
//!   - [`FromTagPresence`] (§8.1.1.3) — a request carries a From-tag.
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

/// **§8.1.1.3 — a UAC tags the From of every request.** The From-tag is the
/// UAC's half of the §12 dialog identifier; a request without one, or with an
/// empty one, names no dialog the peer can match, inside a dialog or out. A
/// CANCEL and an ACK copy the From of the request they follow, tag included.
///
/// The occasion is one fresh request the endpoint sent, read off the raw From
/// row. A vantage with no header block opens no occasion.
///
/// Charges the sender, which mints the tag.
pub struct FromTagPresence;

impl Obligation for FromTagPresence {
    fn id(&self) -> RuleId {
        RuleId::FromTagPresence
    }

    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            if msg.repeat {
                continue;
            }
            let Kind::Request { method } = &msg.kind else { continue };
            let Some(head) = msg.head.as_deref() else { continue };
            let decision = if sniff::from_tag(head).is_empty() {
                Decision::Violated(Evidence::RequiredHeaderAbsent {
                    absent_msg: mi,
                    absent_hop: msg.hop,
                    absent_ts_us: msg.at_us,
                    on: method.clone(),
                    header: "From;tag".to_string(),
                })
            } else {
                Decision::Compliant
            };
            out.push(charge(RuleId::FromTagPresence, msg, mi, decision));
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
/// B2BUA leg marker (`callRef=` / `leg=`) is judged. Charges the endpoint that
/// inserted the row.
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
            if msg.repeat || !matches!(msg.kind, Kind::Request { .. }) || msg.is_request("INVITE") {
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
mod tests;
