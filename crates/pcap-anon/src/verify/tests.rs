use super::*;
use crate::fake::{Seed, GOLDEN_SEED_HEX};
use crate::model;

/// A one-INVITE document as the CLI reads it.
fn document(raw: &str) -> Flows {
    let summary = model::Summary::Request {
        method: "INVITE".into(),
        uri: "sip:+2822345678@example.net".into(),
        cseq: model::CSeq { seq: 41001, method: "INVITE".into() },
        from: model::Party { uri: "sip:+15552234567@10.0.0.1".into(), tag: Some("t1".into()) },
        to: model::Party { uri: "sip:+2822345678@example.net".into(), tag: None },
    };
    let msg =
        crate::testflow::message(1, "10.0.0.1:5060", "10.0.0.2:5060", raw.as_bytes(), summary);
    let mut leg = crate::testflow::bare_leg("c1", vec![msg]);
    leg.hops = vec![model::Hop { a: "10.0.0.1:5060".into(), b: "10.0.0.2:5060".into() }];
    let mut flows = crate::testflow::document(vec![leg]);
    flows.groups = crate::testflow::flows(&[crate::testflow::leg("c1", &[])], &[&[0]]).groups;
    model::ingest_str(&model::to_json_string(&flows, &crate::testflow::reading()).unwrap()).unwrap()
}

const PAYLOAD: &str = "eyJhdHRlc3QiOiJBIiwiZGVzdCI6eyJ0biI6WyIrMjgyMjM0NTY3OCIsIisyODYyMzQ1Njc4Il19LCJpYXQiOjE0NDMyMDgzNDUsIm9yaWciOnsidG4iOiIrMTU1NTIyMzQ1NjcifSwib3JpZ2lkIjoiZGUzMDVkNTQtNzViNC00MzFiLWFkYjItZWI2YjllNTQ2MDE0In0";

fn invite() -> String {
    format!(
        "INVITE sip:+2822345678@example.net SIP/2.0\r\n\
         Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK1\r\n\
         From: <sip:+15552234567@10.0.0.1>;tag=t1\r\n\
         To: <sip:+2822345678@example.net>\r\n\
         Refer-To: <sip:+83571234567890@10.0.0.9>\r\n\
         P-Vendor: <tel:2210>\r\n\
         Contact: <sip:41001@10.0.0.1>\r\n\
         Identity: eyJhbGciOiJFUzI1NiJ9.{PAYLOAD}.c2ln;info=<https://cert.example/a.pem>\r\n\
         User-to-User: 04303632333435363738;encoding=hex\r\n\
         Call-ID: c1\r\nCSeq: 41001 INVITE\r\nMax-Forwards: 70\r\nContent-Length: 0\r\n\r\n"
    )
}

/// The raw document against itself: every needle survives — the URI
/// ones, the vendor `tel:`, the transfer target, the PASSporT-only
/// number, the national number inside the hex UUI — except the one whose
/// only whole-run occurrence outside the CSeq value is inside a longer
/// run.
#[test]
fn survivors_are_counted_as_whole_runs_outside_the_cseq_value() {
    let plan = crate::testflow::plan();
    let raw = document(&invite());
    assert_eq!(
        survivors(&raw, &raw, &plan, &Scope::default()),
        7,
        "2822345678, 15552234567, 83571234567890, 2210, 41001, 2862345678, 062345678 (the UUI)"
    );
    // The UUI's number is searched decoded: rewritten hex hides it, and
    // the hex text's own digit runs are never needles' haystack.
    let uui_rewritten = document(&invite().replace("04303632333435363738", "04303030393132333435"));
    assert_eq!(survivors(&raw, &uui_rewritten, &plan, &Scope::default()), 6);

    // The Contact key equals the CSeq value: only the CSeq occurrence
    // kept, the Contact rewritten — no survivor.
    let rewritten = document(&invite().replace("sip:41001@", "sip:00091@"));
    assert_eq!(survivors(&raw, &rewritten, &plan, &Scope::default()), 6);
    // A needle inside a longer run is not a survivor; as a whole run in a
    // Call-ID it is.
    let glued = document(&invite().replace("sip:41001@", "sip:141001@"));
    assert_eq!(survivors(&raw, &glued, &plan, &Scope::default()), 6);
    let call_id = document(
        &invite().replace("sip:41001@", "sip:00091@").replace("Call-ID: c1", "Call-ID: 41001"),
    );
    assert_eq!(survivors(&raw, &call_id, &plan, &Scope::default()), 7);
    // A PASSporT number is searched in the decoded payload, and the
    // base64 text's own digit runs are not searched.
    let base64_only = document(&invite().replace(
        "Refer-To: <sip:+83571234567890@10.0.0.9>\r\n",
        "Refer-To: <sip:+83500091234567@10.0.0.9>\r\n",
    ));
    assert_eq!(survivors(&raw, &base64_only, &plan, &Scope::default()), 6);
    let no_passport = document(
        &invite().replace(PAYLOAD, "eyJhdHRlc3QiOiJBIn0").replace("+2822345678", "+2800098273"),
    );
    assert_eq!(survivors(&raw, &no_passport, &plan, &Scope::default()), 5);
}

/// The anonymizer's own output has no survivor.
#[test]
fn an_anonymised_document_has_no_survivor() {
    let plan = crate::testflow::plan();
    let seed = Seed::from_hex(GOLDEN_SEED_HEX).unwrap();
    let raw = document(&invite());
    let mut anonymised = document(&invite());
    let report = crate::anonymize::anonymize(&mut anonymised, &plan, &seed).unwrap();
    // The UUI's national number is the PASSporT's `dest` NSN: one key.
    assert_eq!(report.entries.len(), 6);
    assert_eq!(survivors(&raw, &anonymised, &plan, &Scope::default()), 0);
}

/// A display name that still spells a letter other than the masks or a
/// non-ASCII byte is counted, once per distinct name; a frozen name
/// (`anonymous`, a `urn:`) and a masked one are not, and the anonymiser's
/// output counts none.
#[test]
fn unmasked_display_names_are_counted_once_each() {
    let named = invite()
        .replace("From: <", "From: \"Фейк\" <")
        .replace("To: <", "To: \"Anonymous\" <")
        .replace("Contact: <", "Contact: Foo <")
        .replace("Refer-To: <", "Refer-To: \"Фейк\" <")
        .replace("P-Vendor: <tel:2210>\r\n", "Diversion: \"Xxx 00\" <tel:2210>\r\n");
    assert_eq!(unmasked_displays(&document(&named), &Scope::default()), 2);
    let mut anonymised = document(&named);
    let seed = Seed::from_hex(GOLDEN_SEED_HEX).unwrap();
    crate::anonymize::anonymize(&mut anonymised, &crate::testflow::plan(), &seed).unwrap();
    assert_eq!(unmasked_displays(&anonymised, &Scope::default()), 0);
}

/// A composed run whose fake also spells a national number leaves no
/// number of the raw document behind.
#[test]
fn a_composed_run_leaves_no_survivor() {
    let plan = crate::testflow::plan();
    let seed = Seed::from_hex(GOLDEN_SEED_HEX).unwrap();
    let composed = invite()
        .replace("INVITE sip:+2822345678@", "INVITE sip:56781234560000000000@")
        .replace("Contact: <sip:41001@", "Contact: <sip:23456@")
        .replace("P-Vendor: <tel:2210>", "P-Vendor: <tel:5678>");
    let mut anonymised = document(&composed);
    crate::anonymize::anonymize(&mut anonymised, &plan, &seed).unwrap();
    assert_eq!(survivors(&document(&composed), &anonymised, &plan, &Scope::default()), 0);
}

/// A one-byte hex UUI is read as written: its two digits elsewhere in the
/// datagram are no decoded payload, so a needle spelling them stays whole.
#[test]
fn a_hex_uui_too_short_for_a_number_is_not_decoded_into_the_haystack() {
    let plan = crate::testflow::plan();
    let uui = invite().replace("04303632333435363738;encoding=hex", "34;encoding=hex");
    let raw = document(&uui.replace("<tel:2210>", "<tel:3410>"));
    let without = survivors(
        &raw,
        &document(&uui.replace("<tel:2210>", "<tel:0910>")),
        &plan,
        &Scope::default(),
    );
    assert_eq!(survivors(&raw, &raw, &plan, &Scope::default()), without + 1);
}

/// A fake that spells another real number where the raw datagram held a
/// different one is a coincidence; the same number left in place is a
/// survivor.
#[test]
fn a_fake_spelling_another_number_is_no_survivor_and_a_number_left_in_place_is() {
    let plan = crate::testflow::plan();
    let raw = document(&invite().replace("<tel:2210>", "<tel:52002>"));
    let both = invite().replace("<tel:2210>", "<tel:00093>").replace("sip:41001@", "sip:00091@");
    let base = survivors(&raw, &document(&both), &plan, &Scope::default());
    let coincidence = both.replace("sip:00091@", "sip:52002@");
    assert_eq!(survivors(&raw, &document(&coincidence), &plan, &Scope::default()), base);
    let left = both.replace("<tel:00093>", "<tel:52002>");
    assert_eq!(survivors(&raw, &document(&left), &plan, &Scope::default()), base + 1);
}

/// A redacted PASSporT leaves the datagram's width as it was: a fake
/// spelling another number of the raw document is a coincidence there too,
/// and the same number left in place is a survivor.
#[test]
fn a_redacted_passport_keeps_the_place_check_of_every_other_run() {
    let plan = crate::testflow::plan();
    let raw = document(&invite().replace("<tel:2210>", "<tel:52002>"));
    let redacted = crate::fake::names::redact(PAYLOAD);
    let both = invite()
        .replace(PAYLOAD, &redacted)
        .replace("<tel:2210>", "<tel:00093>")
        .replace("sip:41001@", "sip:00091@");
    let base = survivors(&raw, &document(&both), &plan, &Scope::default());
    let coincidence = both.replace("sip:00091@", "sip:52002@");
    assert_eq!(survivors(&raw, &document(&coincidence), &plan, &Scope::default()), base);
    let left = both.replace("<tel:00093>", "<tel:52002>");
    assert_eq!(survivors(&raw, &document(&left), &plan, &Scope::default()), base + 1);
}

/// A phone number left in an `o=` sess-id or sess-version is a survivor, a
/// counter or not; a short code's form or a narrower run there is a counter.
/// `2862345678` is the PASSporT `dest` number, a national number.
#[test]
fn a_phone_number_left_in_an_origin_is_a_survivor() {
    let plan = crate::testflow::plan();
    let sdp = |origin: &str| {
        invite().replace(
            "Content-Length: 0\r\n\r\n",
            &format!("Content-Length: 0\r\n\r\nv=0\r\n{origin}\r\n"),
        )
    };
    let raw = document(&invite().replace("<tel:2210>", "<sip:+2839491@h>"));
    let base = survivors(&raw, &document(&sdp("o=- 1 2 IN IP4 h")), &plan, &Scope::default());
    let version = document(&sdp("o=- 1 2839491 IN IP4 h"));
    assert_eq!(
        survivors(&raw, &version, &plan, &Scope::default()),
        base,
        "a short code's composed form is no phone number"
    );
    let header = document(&sdp("o=- 1 2 IN IP4 h").replace("<tel:2210>", "<sip:+2839491@h>"));
    assert_eq!(survivors(&raw, &header, &plan, &Scope::default()), base + 1);
    let national =
        document(&sdp("o=- 2862345678 1 IN IP4 h").replace(PAYLOAD, "eyJhdHRlc3QiOiJBIn0"));
    let national_base = document(&sdp("o=- 1 2 IN IP4 h").replace(PAYLOAD, "eyJhdHRlc3QiOiJBIn0"));
    assert_eq!(
        survivors(&raw, &national, &plan, &Scope::default()),
        survivors(&raw, &national_base, &plan, &Scope::default()) + 1
    );
    let short_sess_id = document(&sdp("o=- 2839491 1 IN IP4 h"));
    assert_eq!(survivors(&raw, &short_sess_id, &plan, &Scope::default()), base);
    let national_version =
        document(&sdp("o=- 1 2862345678 IN IP4 h").replace(PAYLOAD, "eyJhdHRlc3QiOiJBIn0"));
    assert_eq!(
        survivors(&raw, &national_version, &plan, &Scope::default()),
        survivors(&raw, &national_base, &plan, &Scope::default()) + 1
    );
    // A run narrower than a phone number is a counter there.
    let narrow = document(&sdp("o=- 39491 1 IN IP4 h"));
    assert_eq!(survivors(&raw, &narrow, &plan, &Scope::default()), base);
}
