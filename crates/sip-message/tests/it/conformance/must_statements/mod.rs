//! The RFC's MUST and MUST NOT statements on message syntax, section by section,
//! and the shapes the RFC and the stated rules name. A row states what the text
//! says: this form reads, or this form is refused, answered or dropped as
//! ADR-0054 gives. ADR-0001's refusals are rows too, and so are the shapes the
//! stack's own rules name (`rule-*`).

use sip_message::header;
use sip_message::{sipfrag, SipMessage};

use super::break_rule::refused;
use super::message::{read, reads_every_header, typed, ReadCheck, Template};
use super::ratchet::{settle, Row};

fn options() -> Template {
    Template::request("OPTIONS")
}

fn request(method: &str) -> Template {
    Template::request(method)
}

fn response(status: u16, method: &str) -> Template {
    Template::response(status, method)
}

/// The message reads and every typed reader reads it.
fn reads(raw: &[u8]) -> Result<(), String> {
    match read(raw) {
        Ok(msg) => reads_every_header(&msg),
        Err(e) => Err(e.reason),
    }
}

fn accept(name: &str, template: Template) -> Row {
    Row::new(format!("must/{name}"), reads(&template.bytes()))
}

fn accept_raw(name: &str, raw: &[u8]) -> Row {
    Row::new(format!("must/{name}"), reads(raw))
}

/// A line that breaks a rule, whose message is answered 400.
fn answered(name: &str, template: Template) -> Row {
    let because = template.subject().map(str::to_string);
    assert!(because.is_some(), "{name}: a start-line row names its cause");
    Row::new(format!("must/{name}"), refused(&template.bytes(), true, because.as_deref()))
}

/// [`answered`], for a reason naming one of `because`'s `|`-separated words: the
/// rows whose line names no header, or whose refusal names the rule broken.
fn answered_because(name: &str, because: &str, template: Template) -> Row {
    Row::new(format!("must/{name}"), refused(&template.bytes(), true, Some(because)))
}

/// [`dropped`], for a reason naming one of `because`'s words.
fn dropped_because(name: &str, because: &str, template: Template) -> Row {
    Row::new(format!("must/{name}"), refused(&template.bytes(), false, Some(because)))
}

/// A line that leaves no partial: echoed, or the message is a response.
fn dropped(name: &str, template: Template) -> Row {
    let because = template.subject().map(str::to_string);
    assert!(because.is_some(), "{name}: a start-line row names its cause");
    Row::new(format!("must/{name}"), refused(&template.bytes(), false, because.as_deref()))
}

/// The line is refused, or the typed reader of a header ingress does not vouch
/// for refuses it.
fn unread(name: &str, template: Template, read_it: ReadCheck) -> Row {
    let outcome = match read(&template.bytes()) {
        Ok(msg) => read_it(&msg).err().map_or(Err("read".to_string()), |_| Ok(())),
        Err(_) => Ok(()),
    };
    Row::new(format!("must/{name}"), outcome)
}

/// `check` holds of the message `template` builds.
fn holds(name: &str, template: Template, check: ReadCheck) -> Row {
    let outcome = match read(&template.bytes()) {
        Ok(msg) => check(&msg),
        Err(e) => Err(e.reason),
    };
    Row::new(format!("must/{name}"), outcome)
}

const NO_TAG_FROM: &str = "From: <sip:alice@example.com>";

/// A line of Digest credentials of `name` in realm `realm`.
fn credentials(name: &str, realm: &str) -> String {
    format!(
        "{name}: Digest username=\"a\", realm=\"{realm}\", nonce=\"n\", \
         uri=\"sip:bob@example.com\", response=\"0123456789abcdef0123456789abcdef\""
    )
}

/// A request of `method` in a dialog, with `line` in place of its own.
fn in_dialog(method: &str, line: &str) -> Template {
    let slot = if line.starts_with("CSeq") { "cseq" } else { "in-dialog" };
    request(method).with("to", "To: <sip:bob@example.com>;tag=b1").with(slot, line)
}

/// The Privacy line reads as the three values `id`, `user`, `critical`.
fn privacy_reads_three(msg: &SipMessage) -> Result<(), String> {
    let privacy = msg.token_set::<header::Privacy>().ok_or("no value")?.map_err(|e| e.reason)?;
    let wire = sip_message::header::HeaderValue::to_wire(&privacy);
    let values: Vec<&str> = wire.split([';', ',']).map(str::trim).collect();
    (values == ["id", "user", "critical"]).then_some(()).ok_or(format!("read {wire}"))
}

/// The Privacy the stack builds from three values separates them with `,`.
fn privacy_written_with_commas() -> Result<(), String> {
    let written = header::Privacy::of(["id", "user", "critical"]);
    let wire = sip_message::header::HeaderValue::to_wire(&written);
    (wire == "id, user, critical").then_some(()).ok_or(format!("written {wire:?}"))
}

/// The message is refused, answered or not.
fn refused_any(name: &str, template: Template) -> Row {
    let outcome = match read(&template.bytes()) {
        Ok(_) => Err("accepted".to_string()),
        Err(_) => Ok(()),
    };
    Row::new(format!("must/{name}"), outcome)
}

fn without_contact(method: &str) -> Template {
    request(method).with("contact", "X-Contact: none")
}

fn response_without_contact(method: &str) -> Template {
    response(200, method).with("contact", "X-Contact: none")
}

mod extensions;
mod section_7;
mod section_8;
mod sections_18_to_25;

#[test]
fn every_must_statement_holds() {
    let mut rows = section_7::rows();
    rows.extend(section_8::rows());
    rows.extend(sections_18_to_25::rows());
    rows.extend(sections_18_to_25::bounds());
    rows.extend(extensions::rows());
    settle("must", &rows);
}
