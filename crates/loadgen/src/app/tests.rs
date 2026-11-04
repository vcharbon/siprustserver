use std::net::SocketAddr;

use super::mix::resolve_profile_mix;
use super::*;
use crate::{CallTuning, Correlation, LoadCase, MixEntry, ShapeRegistry};

/// `Args::parse_from` records exactly the explicitly-passed flags — the
/// discriminator the flag-overrides-profile precedence keys off.
#[test]
#[ignore = "slow lane: loadgen"]
fn parse_records_explicit_flags() {
    let args = Args::parse_from(["loadgen", "--cps", "30", "--drop"]);
    assert!(args.explicit("cps"));
    assert!(args.explicit("drop"));
    assert!(!args.explicit("duration"));
    assert!(!args.explicit("drop_rate"));
    assert_eq!(args.cps, 30.0);
    assert!(args.drop);
}

/// `--shared-socket` synthesizes one address for alice, bob and charlie:
/// the base port, on the bind IP.
#[test]
#[ignore = "slow lane: loadgen"]
fn shared_socket_binds_every_role_on_the_base_port() {
    let args = Args::parse_from(["loadgen", "--shared-socket", "--base-port", "7400"]);
    let cfg = endpoint_config(&args, 5000);
    let base: SocketAddr = "127.0.0.1:7400".parse().unwrap();
    for role in ["alice", "bob", "charlie"] {
        assert_eq!(cfg.addr(role), base, "{role} binds the shared base port");
    }
}

/// `--correlate from-user` selects the From-user strategy (nothing stamped).
#[test]
#[ignore = "slow lane: loadgen"]
fn correlate_from_user_selects_the_from_user_strategy() {
    let args = Args::parse_from(["loadgen", "--correlate", "from-user"]);
    let stamp = correlation(&args).map(|c| c.stamp("+1555010"));
    assert!(
        matches!(stamp, Ok(crate::CorrelationStamp::FromUser)),
        "from-user must select the From-user stamp, got {stamp:?}"
    );
}

/// `--correlate from-user --correlation-extract` keys on the regex's group 1;
/// a regex without a capture group is refused at startup.
#[test]
#[ignore = "slow lane: loadgen"]
fn correlate_from_user_takes_the_extract_regex() {
    let args = |re: &str| {
        Args::parse_from(["loadgen", "--correlate", "from-user", "--correlation-extract", re])
    };
    let c = correlation(&args(r"^(\+?[0-9]+)")).unwrap();
    assert_eq!(c.caller_key(Some("<sip:+1555010;ext=4@pool.example>")).as_deref(), Ok("+1555010"));
    assert!(correlation(&args("[0-9]+")).is_err(), "no capture group");
}

/// `--correlate call-id` selects the Call-ID stamp and needs an explicit
/// `--correlation-extract`: without one the run is refused at startup.
#[test]
#[ignore = "slow lane: loadgen"]
fn correlate_call_id_needs_an_extract_regex() {
    let bare = Args::parse_from(["loadgen", "--correlate", "call-id"]);
    assert!(correlation(&bare).is_err(), "call-id without --correlation-extract");

    let args = Args::parse_from([
        "loadgen",
        "--correlate",
        "call-id",
        "--correlation-extract",
        "^leg[0-9]+-(.+)$",
    ]);
    let c = correlation(&args).unwrap();
    assert!(c.is_call_id());
    assert!(matches!(c.stamp("lg1"), crate::CorrelationStamp::CallId));
}

/// From-user correlation refuses a mix with a case-less entry at startup;
/// other strategies accept it.
#[test]
#[ignore = "slow lane: loadgen"]
fn from_user_mix_needs_a_case_on_every_entry() {
    let registry = ShapeRegistry::with_defaults();
    let inputs = ScenarioInputs::default();
    let entry = |id| MixEntry::by_id(&registry, id, &inputs, 1.0).unwrap();
    let case: e2e_model::TestCase = serde_json::from_str(
        r#"{ "id": "c", "compatibleShapes": ["basic_call"],
                 "input": { "core": { "from": "sip:+15550100@pool.example" } } }"#,
    )
    .unwrap();
    let case = Arc::new(LoadCase::new(case, &Default::default(), 1).unwrap());

    let mix = vec![entry("basic_call").with_case(Some(case)), entry("reinvite")];
    let err = check_from_user_mix(&Correlation::from_user(), &mix).unwrap_err();
    assert!(err.contains("reinvite") && !err.contains("basic_call"), "{err}");
    assert!(check_from_user_mix(&Correlation::from_user(), &mix[..1]).is_ok());
    assert!(check_from_user_mix(&Correlation::to_user(), &mix).is_ok());
}

/// Two mix entries attaching one case file draw distinct identities: the
/// case's `${seq}` walk is shared, never replayed in lockstep per entry.
#[test]
#[ignore = "slow lane: loadgen"]
fn two_entries_on_one_case_draw_distinct_identities() {
    let dir = std::env::temp_dir().join(format!("loadgen-shared-case-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("case.json");
    std::fs::write(
        &path,
        r#"{ "id": "c", "compatibleShapes": ["basic_call"],
                 "input": { "core": { "from": "sip:+1555${seq:4}@pool.example" } } }"#,
    )
    .unwrap();
    let registry = ShapeRegistry::with_defaults();
    let inputs = ScenarioInputs::default();
    let check_sets = Default::default();
    let mut cases = CaseLoader::new(&check_sets, 7);
    let spec = |shape: &str| e2e_model::MixSpec {
        shape: shape.to_string(),
        weight: 1.0,
        case: Some(path.clone()),
        drop_rate: None,
        retransmit: None,
    };
    let base = CallTuning::default();
    let (a, _) = resolve_profile_mix(&spec("basic_call"), base, &registry, &inputs, &mut cases);
    let (b, _) = resolve_profile_mix(&spec("basic_call_em"), base, &registry, &inputs, &mut cases);
    let from = |e: &MixEntry| e.case.as_ref().unwrap().resolve().core.from.unwrap();
    let (fa, fb) = (from(&a), from(&b));
    let _ = std::fs::remove_dir_all(&dir);
    assert_ne!(fa, fb, "both entries drew the same identity");
}
