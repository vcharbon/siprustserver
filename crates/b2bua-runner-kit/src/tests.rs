use super::*;

fn suffixes(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn ip_literal_dest_is_admissible_regardless_of_suffixes() {
    // The default B2BUA_DEST (127.0.0.1) and any IP literal must boot even
    // with an empty allow-list.
    assert!(validate_default_dest("127.0.0.1", &suffixes(&[])).is_ok());
}

#[test]
fn dest_matching_a_configured_suffix_boots() {
    assert!(validate_default_dest(
        "sipp-uas.sip-test.svc.cluster.local",
        &suffixes(&[".svc.cluster.local"])
    )
    .is_ok());
}

#[test]
fn dest_excluded_by_its_own_allow_list_refuses_boot() {
    let e = validate_default_dest("bob", &suffixes(&[".svc.cluster.local"]))
        .expect_err("a default callee its own allow-list rejects must fail boot");
    assert!(e.contains("B2BUA_DEST"), "msg was: {e}");
}

#[test]
fn star_wildcard_admits_any_dest() {
    assert!(validate_default_dest("bob", &suffixes(&["*"])).is_ok());
}

#[test]
fn ingress_brake_pct_out_of_range_refuses_boot() {
    assert!(validate_ingress_brake_pct(0).is_err());
    assert!(validate_ingress_brake_pct(101).is_err());
    // boundaries are in range
    assert!(validate_ingress_brake_pct(1).is_ok());
    assert!(validate_ingress_brake_pct(100).is_ok());
}

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let pairs: Vec<(String, String)> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |key| pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
}

/// The privacy-service switch is on unless stated off; a value that is
/// neither on nor off refuses the boot rather than picking a side.
#[test]
fn the_privacy_service_switch_defaults_on_and_reads_off() {
    let read = |v: Option<&str>| switch("B2BUA_PRIVACY_SERVICE", v.map(String::from), true);
    assert!(read(None));
    assert!(read(Some(" ")));
    assert!(read(Some("on")));
    assert!(!read(Some("off")));
    assert!(!read(Some("0")));
    assert!(!read(Some("FALSE")));
    assert!(std::panic::catch_unwind(|| read(Some("of"))).is_err());
}

#[test]
fn a_blank_value_states_nothing() {
    assert_eq!(stated(None), None);
    assert_eq!(stated(Some(String::new())), None);
    assert_eq!(stated(Some("  ".into())), None);
    assert_eq!(stated(Some(" 8 ".into())), Some("8".into()));
    assert_eq!(stated_list(Some(" , ".into())), None);
    assert_eq!(stated_list(Some("Allow, ".into())), Some(vec!["Allow".to_string()]));
}

#[test]
fn the_ring_knobs_read_blank_as_unset() {
    assert_eq!(cdr_ring_from_lookup(lookup(&[])), (0, vec![]));
    assert_eq!(
        cdr_ring_from_lookup(lookup(&[
            ("B2BUA_CDR_MESSAGE_RING", ""),
            ("B2BUA_CDR_CAPTURED_HEADERS", " , ")
        ])),
        (0, vec![])
    );
    assert_eq!(
        cdr_ring_from_lookup(lookup(&[
            ("B2BUA_CDR_MESSAGE_RING", " 16 "),
            ("B2BUA_CDR_CAPTURED_HEADERS", "Allow, Accept")
        ])),
        (16, vec!["Allow".to_string(), "Accept".to_string()])
    );
}

#[test]
#[should_panic(expected = "B2BUA_CDR_MESSAGE_RING")]
fn an_unparsable_ring_cap_refuses_the_boot() {
    cdr_ring_from_lookup(lookup(&[("B2BUA_CDR_MESSAGE_RING", "lots")]));
}

#[test]
fn split_host_port_defaults_and_parses() {
    assert_eq!(split_host_port("bob:5071"), ("bob".to_string(), 5071));
    assert_eq!(split_host_port("bob"), ("bob".to_string(), 5060));
    assert_eq!(split_host_port("bob:nope"), ("bob".to_string(), 5060));
}

#[test]
fn require_outbound_proxy_without_proxy_refuses_boot() {
    let e = validate_outbound_proxy_requirement(true, false)
        .expect_err("required-but-unset outbound proxy must fail boot");
    assert!(e.contains("B2BUA_OUTBOUND_PROXY"), "msg was: {e}");
    assert!(e.contains("B2BUA_REQUIRE_OUTBOUND_PROXY"), "msg names the guard: {e}");
}

#[test]
fn require_outbound_proxy_with_proxy_boots() {
    assert!(validate_outbound_proxy_requirement(true, true).is_ok());
}

#[test]
fn outbound_proxy_optional_when_not_required() {
    assert!(validate_outbound_proxy_requirement(false, false).is_ok());
    assert!(validate_outbound_proxy_requirement(false, true).is_ok());
}

#[test]
fn truthy_flag_grammar() {
    for t in ["1", "true", "YES", " on "] {
        assert!(is_truthy(t), "{t:?} must be truthy");
    }
    for f in ["0", "", "off", "nope"] {
        assert!(!is_truthy(f), "{f:?} must be falsy");
    }
}
