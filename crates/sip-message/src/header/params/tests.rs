use super::*;

fn header_params(v: &str) -> Params {
    read_params(&SipStr::owned(v), 0, ParamReading::Strict, None).unwrap().0
}

fn strict(v: &str) -> Result<Params, SipParseError> {
    read_params(&SipStr::owned(v), 0, ParamReading::Strict, None).map(|(p, _)| p)
}

#[test]
fn wire_order_and_spelling_survive() {
    let p = header_params(";Tag=abc;LR;q=0.5");
    let names: Vec<&str> = p.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(names, ["Tag", "LR", "q"]);
    assert_eq!(p.value("tag"), Some("abc"));
    assert!(p.get("lr").unwrap().is_flag());
}

/// The lenient reading keeps an empty value apart from a flag; the strict
/// one refuses it, `gen-value` being no empty run.
#[test]
fn a_flag_is_distinct_from_an_empty_value() {
    let (p, _) = read_params(&SipStr::owned(";lr;maddr="), 0, ParamReading::Lenient, None).unwrap();
    assert_eq!(p.get("lr"), Some(&ParamValue::Flag));
    assert_eq!(p.get("maddr"), Some(&ParamValue::Token(SipStr::EMPTY)));
    assert!(strict(";lr;maddr=").is_err());
}

/// A name appears once (RFC 3261 §7.3.1), whatever its case and value, in a
/// short list and in one of thousands, which is read in one pass.
#[test]
fn a_repeated_name_is_refused() {
    assert!(strict(";a=1;A=1").is_err());
    assert!(strict(";a=1;a=2").is_err());
    assert!(strict(";lr;lr").is_err());
    let long: String = (0..3000).map(|n| format!(";p{n}=1")).collect();
    assert_eq!(strict(&long).map(|p| p.len()).ok(), Some(3000));
    assert!(strict(&format!("{long};p7=1")).is_err());
}

/// Off the grammar: junk between parameters, a name that is no token, an
/// open quote, a value that is no token, host or quoted-string.
#[test]
fn a_list_off_the_grammar_is_refused_strictly_and_cut_up_to_its_first_bad_part() {
    for raw in [";a=1 junk;b=2", ";a(b=c", ";x=\"abc", ";x=a b", ";=1", ";x=@"] {
        assert!(strict(raw).is_err(), "{raw}");
    }
    let (cut, _) =
        read_params(&SipStr::owned(";a=1;b=<x>;c=3"), 0, ParamReading::UpToFirstBad, None).unwrap();
    assert_eq!(cut.len(), 1);
    let (all, _) =
        read_params(&SipStr::owned(";a=1 junk;b=2"), 0, ParamReading::Lenient, None).unwrap();
    assert_eq!(all.value("b"), Some("2"));
}

/// `received` takes an unbracketed IPv6 address where a reader names it.
#[test]
fn a_named_parameter_takes_an_unbracketed_ipv6_address() {
    let raw = SipStr::owned(";received=2001:db8::9;rport");
    let (p, _) = read_params(&raw, 0, ParamReading::Strict, Some("received")).unwrap();
    assert_eq!(p.value("received"), Some("2001:db8::9"));
    assert!(strict(";received=2001:db8::9").is_err());
    assert!(read_params(&raw, 0, ParamReading::Strict, Some("maddr")).is_err());
}

#[test]
fn a_quoted_value_keeps_its_separators() {
    let p = header_params(r#";reason="a;b,c";next=1"#);
    assert_eq!(p.value("reason"), Some("a;b,c"));
    assert_eq!(p.value("next"), Some("1"));
    let mut w = Wire::new();
    p.render(&mut w);
    assert_eq!(w.as_str(), r#";reason="a;b,c";next=1"#);
}

#[test]
fn set_replaces_in_place_and_append_keeps_position() {
    let mut p = header_params(";branch=z1;rport");
    p.set("BRANCH", ParamValue::Token(SipStr::owned("z2")));
    p.set("received", ParamValue::Token(SipStr::owned("1.2.3.4")));
    let mut w = Wire::new();
    p.render(&mut w);
    assert_eq!(w.as_str(), ";branch=z2;rport;received=1.2.3.4");
}

// The P-Charging-Vector shape: the value IS the parameter list, so the
// first item carries no leading `;`.
#[test]
fn a_whole_value_parameter_list_reads_its_leading_item() {
    let p = Params::parse_list(&SipStr::owned(
        r#"icid-value="a;b";icid-generated-at=10.0.0.1;orig-ioi=x"#,
    ));
    assert_eq!(p.value("ICID-Value"), Some("a;b"));
    assert_eq!(p.value("orig-ioi"), Some("x"));
    assert_eq!(p.get("term-ioi"), None);
}

// A leading token (the Reason shape) reads as a flag, so the parameters
// after it still answer.
#[test]
fn a_leading_token_does_not_hide_the_parameters_after_it() {
    let p = Params::parse_list(&SipStr::owned("Q.850;cause=16;text=\"normal\""));
    assert_eq!(p.get("Q.850"), Some(&ParamValue::Flag));
    assert_eq!(p.value("cause"), Some("16"));
    assert_eq!(p.value("text"), Some("normal"));
}
