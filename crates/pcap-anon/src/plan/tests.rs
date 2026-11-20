use super::*;

fn nsn(cc: &str, nsn: &str) -> NumberClass {
    NumberClass::Nsn { cc: cc.into(), nsn: nsn.into() }
}

/// Every composed form of the same default-country number reaches the same
/// NSN: E.164, service-prefixed, trunk-composed, `00CC` and national. A
/// foreign number embedded as `+28 00 1 <NSN>`, a bare `CCNSN` with Contact
/// junk behind it and a three-digit CC classify under their own CC.
#[test]
fn corpus_composed_forms_all_reach_one_nsn() {
    let p = crate::testflow::plan();
    for user in
        ["+2822345678", "+281555922345678", "+15550102822345678", "002822345678", "022345678"]
    {
        assert_eq!(p.classify(user), Some(nsn("28", "22345678")), "{user}");
    }
    assert_eq!(p.classify("+280015551234567"), Some(nsn("1", "5551234567")));
    assert_eq!(p.classify("15552234567.iIiIiI.500cc7a7."), Some(nsn("1", "5552234567")));
    assert_eq!(p.classify("+899123456789012"), Some(nsn("899", "123456789012")));
}

/// A capture dialling over a second ingress trunk `15551234500` (beside the
/// `15550` family) carries in its trunk-composed R-URI the called NSN its To
/// header's service-composed form carries; both reach one NSN key, the trunk
/// digits kept verbatim. The anonymised image (the called NSN's fake behind
/// the trunk) still classifies over the trunk, so the resolved R-URI maps
/// positionally instead of freezing.
///
/// ```text
/// R-URI (trunk-composed)   +15551234500 1 5551234567 = +1555123450015551234567
/// To    (service-composed) +28 00       1 5551234567 = +280015551234567
/// ```
#[test]
fn a_second_ingress_trunk_composed_ruri_maps_to_called() {
    let p = crate::testflow::plan();
    assert_eq!(p.classify("+1555123450015551234567"), Some(nsn("1", "5551234567")));
    assert_eq!(p.classify("+280015551234567"), Some(nsn("1", "5551234567")));
    assert_eq!(p.classify("+1555123450010009000003"), Some(nsn("1", "0009000003")));
}

#[test]
fn a_trunk_composed_user_part_names_its_trunk_id() {
    let p = crate::testflow::plan();
    assert_eq!(p.trunk_id("+15550102822345678"), Some("1555010"));
    assert_eq!(p.trunk_id("0015550102822345678"), Some("1555010"));
    assert_eq!(p.trunk_id("+1555123450015551234567"), Some("15551234500"));
    assert_eq!(p.trunk_id("+15550102800098273"), Some("1555010"), "a fake tail too");
    assert_eq!(p.trunk_id("+2822345678"), None);
    assert_eq!(p.trunk_id("+1555010282960822"), None, "no CCNSN behind the trunk");
    assert_eq!(p.trunk_id("1555010"), None);
}

#[test]
fn private_short_numbers_require_all_digit_user_parts() {
    let p = crate::testflow::plan();
    assert_eq!(p.classify("5550123"), Some(NumberClass::Private { digits: "5550123".into() }));
    assert_eq!(p.classify("55512"), Some(NumberClass::Private { digits: "55512".into() }));
    assert_eq!(p.classify("5551"), Some(NumberClass::Private { digits: "5551".into() }));
    assert_eq!(p.classify("500cc7a7"), None, "digits and junk");
    assert_eq!(p.classify("user"), None);
    assert_eq!(p.classify("555"), None, "below private_len.min");
    assert_eq!(p.classify("55501234"), None, "above private_len.max");
}

/// Every country of the plan classifies under every composed form like the
/// default one; a short code behind the default CC is keyed by its digits.
#[test]
fn every_cc_and_a_short_code_classify() {
    let p = crate::testflow::plan();
    for (user, cc, n) in [
        ("+15552234567", "1", "5552234567"),
        ("+899123456789012", "899", "123456789012"),
        ("0015552234567", "1", "5552234567"),
        ("00899123456789012", "899", "123456789012"),
        ("+1555001899123456789012", "899", "123456789012"),
    ] {
        assert_eq!(p.classify(user), Some(nsn(cc, n)), "{user}");
    }
    assert_eq!(
        p.classify("+2839125"),
        Some(NumberClass::Short { cc: "28".into(), digits: "39125".into() })
    );
    assert_eq!(p.classify("+28391256"), None, "six digits behind +28 is no short code");
    assert_eq!(p.classify("+139125"), None, "1 declares no short codes");
}

/// The total classification: what `classify` leaves is unrecognised over
/// the whole leading run when it is at least four digits wide, and its
/// kept head shrinks with the width. A known CC is the head, whole: one
/// digit for 1, two for 28, three for 899.
#[test]
fn unrecognised_numbers_keep_a_head_the_width_admits() {
    let p = crate::testflow::plan();
    let unrec = |d: &str| Some(NumberClass::Unrecognised { digits: d.into() });
    for user in ["+8351234567890", "+28391256", "+8352370200009001", "123456789", "1234abc"] {
        assert_eq!(p.classify(user), None, "{user}");
        let digits = user.trim_start_matches('+').trim_end_matches("abc");
        assert_eq!(p.classify_any(user), unrec(digits), "{user}");
    }
    assert_eq!(p.classify_any("500cc7a7"), None, "three digits bear no number");
    assert_eq!(p.classify_any("user"), None);
    assert_eq!(p.classify_any("+2839125"), p.classify("+2839125"), "recognised forms first");
    for (width, kept) in [(4, 0), (5, 0), (6, 1), (7, 2), (8, 3), (14, 3), (21, 3)] {
        let class = NumberClass::Unrecognised { digits: "4".repeat(width) };
        assert_eq!(p.kept_prefix(&class), kept, "width {width}");
    }
    for (digits, kept) in
        [("15551234567890", 1), ("2812345678901", 2), ("89912345", 3), ("286123", 1)]
    {
        let class = NumberClass::Unrecognised { digits: digits.into() };
        assert_eq!(p.kept_prefix(&class), kept, "{digits}");
    }
    assert_eq!(p.kept_prefix(&nsn("28", "22345678")), 0);
}

/// The reserved forms a fake takes: the standard `000 9 …` and the short
/// `09xx`.
#[test]
fn the_reserved_forms_are_the_fake_space() {
    let p = crate::testflow::plan();
    for fake in ["000912345", "00091", "0009", "0912", "0900"] {
        assert!(p.is_fake(fake), "{fake}");
    }
    for real in ["22345678", "00012", "00057073", "09123", "0091", "1201", "5551"] {
        assert!(!p.is_fake(real), "{real}");
    }
}

/// Idempotence hinges on this: no composed form of a FAKE number ever
/// classifies to a NON-fake key. It either resolves to a fake-space key
/// (starts with fake_prefix + marker, which discovery skips) or is rejected
/// as a phantom (a 0-leading composed tail, or a tail behind a fake-prefix
/// infix, is not a real NSN). The forms: `+CC` and a fake, a trunk-composed
/// fake, a service-prefixed one, a short fake, a fake behind up to
/// `max_composed_prefix` junk digits.
#[test]
fn fake_forms_are_never_rediscovered() {
    let p = crate::testflow::plan();
    let mut users = vec![
        "+2800098273".to_string(),
        "+15550102800098273".to_string(),
        "+281555900098273".to_string(),
        "0009004".to_string(),
    ];
    for junk in 1..=6 {
        users.push(format!("+2800098273{}", "234567".split_at(junk).0));
    }
    for user in &users {
        if let Some(key) = p.classify(user) {
            assert!(key.key().starts_with(&p.fake_space()), "{user} -> {key:?}");
        }
    }
    assert_eq!(p.classify("000098273"), Some(nsn("28", "00098273")), "the national form");
    assert_eq!(p.classify("+1555001280009827323"), Some(nsn("28", "00098273")));
    assert_eq!(p.classify("+280009827323"), Some(nsn("28", "00098273")));
    assert_eq!(p.classify("0040009123"), None, "a `00`-led tail is no national number");
    assert_eq!(
        p.classify_any("0040009123"),
        Some(NumberClass::Unrecognised { digits: "0040009123".into() })
    );
    assert_eq!(p.classify("+28022345678"), Some(nsn("28", "22345678")), "a `0` infix");
    assert_eq!(p.classify("+281555922345678"), Some(nsn("28", "22345678")), "a service infix");
    assert_eq!(p.classify("001234567"), None, "a 0-leading SN is no national number");
}

/// A service-composed form of a fake (`+CC<svc><fake>`) classifies to the
/// fake NSN, so the R-URI maps positionally instead of freezing. The fake
/// tail is self-identifying (`000…`): it is accepted where a genuine
/// 0-leading phantom (`090…`) stays rejected.
#[test]
fn service_composed_fake_tail_is_accepted_not_frozen() {
    let p = crate::testflow::plan();
    assert_eq!(p.classify("+281074700090001"), Some(nsn("28", "00090001")));
    assert_eq!(p.classify("+280009000223"), Some(nsn("28", "00090002")), "a fake behind junk");
}

/// A fake embedded in a longer composed form (`+CC <fake> <junk>`) shifts
/// into a 0-leading tail window on a second pass; that window never
/// re-classifies, so the fake is never mapped again.
#[test]
fn composed_tail_rejects_zero_leading_phantom() {
    let p = crate::testflow::plan();
    assert_eq!(p.classify("+280009000223"), Some(nsn("28", "00090002")));
    assert_eq!(p.classify("+281201234567"), None, "a 0-leading phantom around no fake");
    assert_eq!(p.classify("+282123456782"), Some(nsn("28", "23456782")), "a composed tail");
}

#[test]
fn invalid_plans_are_rejected_loudly() {
    let err = Plan::from_json_str(
        r#"{"default_cc":"28","countries":[],"private_len":null,
                "max_composed_prefix":6,"fake_prefix":"000"}"#,
    )
    .unwrap_err();
    assert!(err.contains("default_cc"), "{err}");
    let err = Plan::from_json_str(
        r#"{"default_cc":"28","countries":[{"cc":"28","nsn_len":8}],
                "private_len":{"min":3,"max":7},"max_composed_prefix":6,"fake_prefix":"000"}"#,
    )
    .unwrap_err();
    assert!(err.contains("private_len"), "{err}");
    let err = Plan::from_json_str(
        r#"{"default_cc":"28","countries":[{"cc":"28","nsn_len":8,"short_len":8}],
                "private_len":null,"max_composed_prefix":6,"fake_prefix":"000"}"#,
    )
    .unwrap_err();
    assert!(err.contains("short_len"), "{err}");
    let err = Plan::from_json_str(
        r#"{"default_cc":"28","countries":[{"cc":"28","nsn_len":8}],
                "private_len":null,"max_composed_prefix":6,"fake_prefix":"0000"}"#,
    )
    .unwrap_err();
    assert!(err.contains("fake_prefix"), "{err}");
}
