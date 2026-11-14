//! A body type the profile states as a control document is read as one only
//! when its part reads as an XML document; any other content of that type is
//! redacted like a part of an unknown type, and verify reads it as free text.
//! Every name is invented.

use super::refusal_probe_tests::{document, wire};
use crate::anonymize::{anonymize_in, Fakes};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::identities::body_types::{BodyReading, BodyType};
use crate::identities::BodyTypes;
use crate::model::Flows;

const CONTROL: &str = "application/vnd.example.ctl";

/// The scope of a profile stating [`CONTROL`] as a control document.
fn scope() -> Scope {
    let row = BodyType::of(CONTROL, BodyReading::Control);
    Scope::default().reading_bodies(&BodyTypes::new(vec![row]).expect("a bare media type"))
}

/// A one-INVITE document whose body is `body` typed [`CONTROL`].
fn control(body: &str) -> Flows {
    let mut doc = document(&[], "c1@cidhost.callerdom.de", body);
    let text = wire(&doc).replacen("application/sdp", CONTROL, 1);
    crate::model::set_wire(&mut doc.legs[0].msgs[0], text.as_bytes());
    doc
}

/// `doc` anonymised under [`scope`], its wire text.
fn anonymised(doc: &Flows) -> String {
    let mut out = doc.clone();
    anonymize_in(&mut out, &crate::testflow::plan(), &Seed::from_bytes([7; 32]), &scope())
        .expect("anonymises");
    wire(&out)
}

#[test]
fn a_control_part_that_is_no_xml_document_is_redacted() {
    let out = anonymised(&control("dest=5550100;name=Zorblat Quintavius\r\n"));
    assert!(!out.contains("Zorblat") && !out.contains("name="), "{out}");
}

#[test]
fn a_control_part_that_is_an_xml_document_is_read() {
    let out = anonymised(&control("<ctl><play name=\"Zorblat Quintavius\"/></ctl>\r\n"));
    assert!(out.contains("<ctl><play name=\"") && !out.contains("Zorblat"), "{out}");
}

#[test]
fn verify_reads_a_control_part_that_is_no_xml_document_as_free_text() {
    let doc = control("dest=5550100;name=Zorblat Quintavius\r\n");
    let found = crate::verify::leaks::unexplained(
        &doc,
        &scope(),
        &Fakes::default(),
        &crate::testflow::reading(),
    );
    assert!(found.iter().any(|(class, token)| *class == "word" && token == b"zorblat"));
}
