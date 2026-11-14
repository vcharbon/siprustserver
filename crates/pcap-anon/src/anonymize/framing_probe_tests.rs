//! A multipart body's bytes outside every part it frames — the preamble, the
//! epilogue, a body its boundary frames nothing of — carry no structure the
//! anonymiser reads: they are redacted whole, their line breaks and the
//! delimiter lines kept, and verify reads them as free text. Every name is
//! invented.

use super::refusal_probe_tests::{document, wire};
use crate::anonymize::{anonymize_in, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::model::Flows;

const NAMES: [&str; 2] = ["Zorblat Quintavius", "Wendeline Okonkwo-Prax"];

/// A one-INVITE document whose body is `body` typed
/// `multipart/mixed;boundary=b`.
fn multipart(body: &str) -> Flows {
    let mut doc = document(&[], "c1@cidhost.callerdom.de", body);
    let text = wire(&doc).replacen("application/sdp", "multipart/mixed;boundary=b", 1);
    crate::model::set_wire(&mut doc.legs[0].msgs[0], text.as_bytes());
    doc
}

/// A body with a name in its preamble and one in its epilogue, around an
/// SDP part.
fn preamble_and_epilogue() -> String {
    format!(
        "{}\r\n--b\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n--b--\r\n{} epilogue\r\n",
        NAMES[0], NAMES[1]
    )
}

/// `doc` anonymised under the test plan, its wire text.
fn anonymised(doc: &Flows) -> String {
    let mut out = doc.clone();
    anonymize_in(&mut out, &crate::testflow::plan(), &Seed::from_bytes([7; 32]), &Scope::default())
        .expect("anonymises");
    assert_eq!(wire(&out).len(), wire(doc).len(), "the datagram keeps its length");
    wire(&out)
}

/// Whether a word of `name` reads as identity-shaped free text of `doc`.
fn verify_reads(doc: &Flows, name: &str) -> bool {
    let found = crate::verify::leaks::unexplained(
        doc,
        &Scope::default(),
        &Fakes::default(),
        &crate::testflow::reading(),
    );
    let word = name.split(' ').next().unwrap_or_default().to_ascii_lowercase();
    found.iter().any(|(class, token)| *class == "word" && token == word.as_bytes())
}

#[test]
fn a_preamble_and_an_epilogue_are_redacted_and_the_framing_stays() {
    let out = anonymised(&multipart(&preamble_and_epilogue()));
    for name in NAMES {
        assert!(!out.contains(name.split(' ').next().unwrap()), "{name} survives:\n{out}");
    }
    assert!(
        out.contains("\r\n--b\r\nContent-Type: application/sdp\r\n\r\nv=0\r\n--b--\r\n"),
        "the framing and the SDP part stay:\n{out}"
    );
}

#[test]
fn a_body_its_boundary_frames_nothing_of_is_redacted_whole() {
    let body = format!("--zz\r\nContent-Type: text/plain\r\n\r\n{}\r\n--zz--\r\n", NAMES[0]);
    let out = anonymised(&multipart(&body));
    assert!(!out.contains("Zorblat") && !out.contains("text/plain"), "{out}");
}

#[test]
fn verify_reads_the_text_outside_every_part() {
    let doc = multipart(&preamble_and_epilogue());
    for name in NAMES {
        assert!(verify_reads(&doc, name), "verify misses {name}");
    }
    let unframed = multipart(&format!("--zz\r\n\r\n{}\r\n--zz--\r\n", NAMES[0]));
    assert!(verify_reads(&unframed, NAMES[0]), "verify misses the unframed body");
}
