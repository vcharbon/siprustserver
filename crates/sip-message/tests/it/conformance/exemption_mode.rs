//! The suite once with a generic exemption set: an exempted shape is accepted when
//! its deployment names it and refused when it does not, the same shape with a
//! different value is refused whatever is named, and nothing else moves.

use sip_message::header::MaxForwards;
use sip_message::hops::forwarded_max_forwards;
use sip_message::parser::custom::CustomParser;
use sip_message::parser::Exemptions;
use sip_message::{read_stored, SipMessage, SipParser, SipParserLimits};

use super::message::{production, Template};
use super::ratchet::{settle, Row};
use super::{break_rule, generated, rfc_examples};

/// The generic names of the shapes a deployment may state.
const NAMES: &[&str] = &[
    "repeat:from.uri:x-dup",
    "repeat:contact.uri:x-dup",
    "bare-user-semicolon",
    "options-body-no-type",
    "max-forwards:absent",
];

/// The parser a deployment stating `names` runs, `Err` while a name reads as none.
fn parser_naming(names: &[&str]) -> Result<CustomParser, String> {
    let exemptions = Exemptions::read(Some(&names.join(",")))?;
    let limits =
        SipParserLimits { at_first_ingress: true, exemptions, ..SipParserLimits::default() };
    Ok(CustomParser::with_limits(limits))
}

/// The largest set of [`NAMES`] that reads today: a name reads once its exemption
/// exists, and its own rows fail until then.
fn the_generic_set() -> CustomParser {
    let names: Vec<&str> =
        NAMES.iter().copied().filter(|name| Exemptions::read(Some(name)).is_ok()).collect();
    if names.is_empty() {
        return production();
    }
    parser_naming(&names).expect("each name reads")
}

fn accepted_by(name: &str, raw: &[u8]) -> Result<(), String> {
    parser_naming(&[name])?.parse(raw).map(|_| ()).map_err(|e| e.reason)
}

fn refused_by(name: &str, raw: &[u8]) -> Result<(), String> {
    match parser_naming(&[name])?.parse(raw) {
        Ok(_) => Err("accepted".to_string()),
        Err(_) => Ok(()),
    }
}

fn refused_by_default(raw: &[u8]) -> Result<(), String> {
    match production().parse(raw) {
        Ok(_) => Err("accepted".to_string()),
        Err(_) => Ok(()),
    }
}

/// A stored image reads under no configuration, so its shape reads whatever a
/// deployment names.
fn reads_as_stored(raw: &[u8]) -> Result<(), String> {
    read_stored(raw).map(|_| ()).map_err(|e| e.reason)
}

/// One shape: the deployment naming it reads `exempted`, one that does not
/// refuses it, a stored read needs no setting, and `still_refused` is refused
/// whatever is named.
fn shape_rows(row: &str, name: &str, exempted: &[u8], still_refused: &[u8]) -> Vec<Row> {
    vec![
        Row::new(format!("exempt/{row}/accepted-when-named"), accepted_by(name, exempted)),
        Row::new(format!("exempt/{row}/refused-by-default"), refused_by_default(exempted)),
        Row::new(format!("exempt/{row}/read-as-stored"), reads_as_stored(exempted)),
        Row::new(
            format!("exempt/{row}/neighbour-refused-when-named"),
            refused_by(name, still_refused),
        ),
    ]
}

/// A request with no Max-Forwards, read under the exemption that names it, holds
/// the count §16.6 states for one: 70.
fn read_as_70(raw: &[u8]) -> Result<(), String> {
    let parsed = parser_naming(&["max-forwards:absent"])?.parse(raw).map_err(|e| e.reason)?;
    let SipMessage::Request(request) = parsed else { return Err("a response".to_string()) };
    let forwarded = forwarded_max_forwards(&request);
    (forwarded == MaxForwards::new(69)).then_some(()).ok_or(format!("forwards {forwarded:?}"))
}

fn options() -> Template {
    Template::request("OPTIONS")
}

fn invite() -> Template {
    Template::request("INVITE")
}

#[test]
fn an_exempted_shape_is_accepted_only_where_it_is_named() {
    let mut rows = shape_rows(
        "repeat-from-uri",
        "repeat:from.uri:x-dup",
        &options().with("from", "From: <sip:alice@example.com;x-dup=1;x-dup=1>;tag=a1").bytes(),
        &options().with("from", "From: <sip:alice@example.com;x-dup=1;x-dup=2>;tag=a1").bytes(),
    );
    rows.extend(shape_rows(
        "repeat-contact-uri",
        "repeat:contact.uri:x-dup",
        &invite().with("contact", "Contact: <sip:alice@192.0.2.10;x-dup=1;x-dup=1>").bytes(),
        &invite().with("contact", "Contact: <sip:alice@192.0.2.10;x-dup=1;x-dup=2>").bytes(),
    ));
    rows.extend(shape_rows(
        "bare-user-semicolon",
        "bare-user-semicolon",
        &options().with("from", "From: sip:a;b=c@example.com;tag=a1").bytes(),
        &options().with("from", "From: sip:a;b=c@;tag=a1").bytes(),
    ));
    rows.extend(shape_rows(
        "options-body-no-type",
        "options-body-no-type",
        &Template::response(200, "OPTIONS").body("a body").bytes(),
        &Template::response(200, "INVITE").body("a body").bytes(),
    ));
    let absent = options().with("max-forwards", "X-Hops: 70").bytes();
    rows.extend(shape_rows(
        "max-forwards-absent",
        "max-forwards:absent",
        &absent,
        &options().with("max-forwards", "Max-Forwards: x").bytes(),
    ));
    rows.push(Row::new("exempt/max-forwards-absent/read-as-70", read_as_70(&absent)));
    settle("exempt", &rows);
}

/// Under the generic set a valid message still reads and a broken one is still
/// refused: an exemption lifts its own shape and nothing else.
#[test]
fn the_generic_set_moves_nothing_else() {
    let parser = the_generic_set();
    let default = production();
    let sets = [rfc_examples::datagrams(), break_rule::datagrams(), generated::datagrams()];
    for raw in sets.iter().flatten() {
        let wanted = default.parse(raw).map(|_| ()).map_err(|e| e.reason);
        let found = parser.parse(raw).map(|_| ()).map_err(|e| e.reason);
        assert_eq!(wanted, found, "{:?}", String::from_utf8_lossy(raw));
    }
}
