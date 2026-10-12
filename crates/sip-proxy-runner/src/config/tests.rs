//! The boot configuration read through a scripted lookup: plain `#[test]`s, no
//! process env, no runtime.

use super::*;

/// A lookup over `pairs`.
fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: std::collections::HashMap<String, String> =
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |key| map.get(key).cloned()
}

/// A complete, valid single-face boot with a fixed pool, plus `extra`.
fn boot(extra: &[(&str, &str)]) -> Result<ProxyConfig, Vec<String>> {
    let mut pairs = vec![
        ("PROXY_LISTEN", "127.0.0.1:5060"),
        ("PROXY_WORKERS", "w0@10.0.0.1:5060"),
        ("PROXY_HMAC_KEY", "0123456789abcdef0123456789abcdef"),
    ];
    pairs.retain(|(k, _)| !extra.iter().any(|(e, _)| e == k));
    pairs.extend_from_slice(extra);
    ProxyConfig::from_lookup(&lookup(&pairs), &|| None)
}

#[test]
fn a_complete_boot_is_accepted() {
    let cfg = boot(&[]).expect("valid");
    assert_eq!(cfg.listen, "127.0.0.1:5060".parse().unwrap());
    assert_eq!(cfg.workers, WorkerSource::Static("w0@10.0.0.1:5060".into()));
}

// ── parse_ext_face_cidrs — dual-face boot rule ───────────────────────────

#[test]
fn single_face_ignores_cidrs() {
    assert_eq!(parse_ext_face_cidrs(false, None).unwrap(), None);
    assert_eq!(parse_ext_face_cidrs(false, Some("10.0.0.0/8")).unwrap(), None);
}

#[test]
fn dual_face_requires_cidrs() {
    let e = parse_ext_face_cidrs(true, None).unwrap_err();
    assert!(e.contains("PROXY_FACE_INT_CIDRS is REQUIRED"), "{e}");
    let e = parse_ext_face_cidrs(true, Some("  ")).unwrap_err();
    assert!(e.contains("REQUIRED"), "blank counts as missing: {e}");
}

#[test]
fn dual_face_rejects_unparseable_cidrs() {
    let e = parse_ext_face_cidrs(true, Some("10.0.0.0/8,bogus")).unwrap_err();
    assert!(e.contains("bad PROXY_FACE_INT_CIDRS"), "{e}");
}

#[test]
fn dual_face_parses_a_valid_list() {
    let f = parse_ext_face_cidrs(true, Some("10.244.0.0/16,172.20.0.0/16")).unwrap().unwrap();
    assert!(f.contains_ip("10.244.1.2".parse().unwrap()));
    assert!(!f.contains_ip("192.168.60.9".parse().unwrap()));
}

// ── LB_* operator overrides ─────────────────────────────────────────────

/// Each `LB_*` value replaces exactly its field; a coherent set boots.
#[test]
fn lb_overrides_replace_their_fields() {
    let cfg = boot(&[
        ("LB_ELU_SOFT", "0.3"),
        ("LB_ELU_HARD", "0.45"),
        ("LB_ELU_CRITICAL", "0.6"),
        ("LB_BAND_HYSTERESIS", "0.05"),
        ("LB_AIMD_DECREASE_FACTOR", "0.4"),
        ("LB_CAP_CEILING_CPS", "150"),
        ("LB_CAP_FLOOR_CPS", "2"),
    ])
    .expect("coherent overrides boot");
    assert_eq!(cfg.observer.elu_soft, 0.3);
    assert_eq!(cfg.observer.elu_hard, 0.45);
    assert_eq!(cfg.observer.elu_critical, 0.6);
    assert_eq!(cfg.observer.aimd_decrease_factor, 0.4);
    assert_eq!(cfg.observer.cap_ceiling_cps, 150.0);
    assert_eq!(cfg.observer.cap_floor_cps, 2.0);
}

/// An incoherent set (reversed bands) refuses the boot.
#[test]
fn lb_overrides_that_reverse_the_bands_refuse_the_boot() {
    let e = boot(&[("LB_ELU_SOFT", "0.8"), ("LB_ELU_HARD", "0.5"), ("LB_ELU_CRITICAL", "0.3")])
        .expect_err("reversed bands");
    assert!(e.join("\n").contains("elu_soft < elu_hard < elu_critical"), "{e:?}");
}

// ── refusals: one per class ──────────────────────────────────────────────

/// `Err` from `boot(extra)`, naming `key`.
fn refused(extra: &[(&str, &str)], key: &str) -> Vec<String> {
    let errors = boot(extra).expect_err("the boot must be refused");
    assert!(errors.iter().any(|e| e.contains(key)), "no problem names {key}: {errors:?}");
    errors
}

#[test]
fn a_malformed_value_refuses_the_boot() {
    refused(&[("HEALTH_INTERVAL_MS", "1s")], "HEALTH_INTERVAL_MS");
}

#[test]
fn an_lb_override_that_does_not_parse_refuses_the_boot() {
    refused(&[("LB_ELU_CRITICAL", "not-a-number")], "LB_ELU_CRITICAL");
}

#[test]
fn an_empty_value_refuses_the_boot() {
    refused(&[("HEALTH_TIMEOUT_MS", "")], "HEALTH_TIMEOUT_MS");
}

#[test]
fn an_out_of_range_value_refuses_the_boot() {
    refused(&[("LB_AIMD_DECREASE_FACTOR", "1.5")], "LB_AIMD_DECREASE_FACTOR");
}

#[test]
fn an_empty_namespace_refuses_the_boot() {
    let get = lookup(&[
        ("PROXY_LISTEN", "127.0.0.1:5060"),
        ("PROXY_HMAC_KEY", "0123456789abcdef0123456789abcdef"),
        ("PROXY_NAMESPACE", ""),
        ("POD_NAMESPACE", "lab"),
    ]);
    let errors = ProxyConfig::from_lookup(&get, &|| Some("lab".into())).expect_err("refused");
    assert!(errors.iter().any(|e| e.contains("PROXY_NAMESPACE")), "{errors:?}");
}

#[test]
fn an_unspecified_listen_address_without_an_advertise_refuses_the_boot() {
    refused(&[("PROXY_LISTEN", "0.0.0.0:5060")], "PROXY_ADVERTISE");
}

#[test]
fn a_name_as_a_bind_address_refuses_the_boot() {
    refused(&[("PROXY_LISTEN", "localhost:5060")], "PROXY_LISTEN");
}

#[test]
fn a_missing_cookie_signing_key_refuses_the_boot() {
    let get = lookup(&[("PROXY_LISTEN", "127.0.0.1:5060"), ("PROXY_WORKERS", "w0@10.0.0.1:5060")]);
    let errors = ProxyConfig::from_lookup(&get, &|| None).expect_err("refused");
    assert!(errors.iter().any(|e| e.contains("PROXY_HMAC_KEY")), "{errors:?}");
}
