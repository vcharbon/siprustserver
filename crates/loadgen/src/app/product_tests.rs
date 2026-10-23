use std::collections::BTreeMap;
use std::net::SocketAddr;

use e2e_model::{EndpointConfig, ShapeDescriptor};

use super::*;

/// A second product: its own flag selects which of its shape sets the run
/// drives, it runs with its own loadgen defaults, and one of its modes sets
/// the correlation unless the command line names one.
struct Other;

#[derive(clap::Args, Debug)]
struct OtherFlags {
    /// Which shape set to drive.
    #[arg(long, default_value = "small")]
    set: String,
}

impl Product for Other {
    type Flags = OtherFlags;

    fn name(&self) -> &'static str {
        "other-loadgen"
    }

    fn about(&self) -> &'static str {
        "another product's load generator"
    }

    fn flag_defaults(&self) -> &'static [(&'static str, &'static str)] {
        &[("ring_delay_ms", "2000"), ("check_sets_dir", "other/checksets")]
    }

    fn configure(&self, flags: &OtherFlags, args: &mut Args) -> Result<(), String> {
        match flags.set.as_str() {
            "small" => Ok(()),
            "numbered" if !args.explicit("correlate") => {
                args.correlate = "to-user".to_string();
                Ok(())
            }
            "numbered" => Ok(()),
            other => Err(format!("--set {other:?}: unknown set (valid: small | numbered)")),
        }
    }

    fn registry(
        &self,
        flags: &OtherFlags,
        layout: &RunLayout<'_>,
    ) -> Result<ShapeRegistry, String> {
        let mut reg = ShapeRegistry::empty();
        let id = match (flags.set.as_str(), layout.shared_socket()) {
            ("small", _) => "other_small",
            (_, true) => "other_numbered_shared",
            (_, false) => "other_numbered",
        };
        reg.register(ShapeDescriptor::new(id));
        Ok(reg)
    }
}

fn layout(alice: &str, bob: &str) -> EndpointConfig {
    let addr = |s: &str| s.parse::<SocketAddr>().unwrap();
    let roles: BTreeMap<String, SocketAddr> = [
        ("alice".to_string(), addr(alice)),
        ("bob".to_string(), addr(bob)),
        ("charlie".to_string(), addr(bob)),
        ("lb".to_string(), addr("127.0.0.1:5060")),
    ]
    .into();
    EndpointConfig {
        schema: None,
        infra_shape: "test".to_string(),
        roles,
        recv_timeout_ms: 1000,
        transit_delay_ms: 0,
        egress: None,
    }
}

#[test]
fn a_product_flag_parses_beside_the_loadgen_flags() {
    let (flags, args) =
        parse_from(&Other, ["other-loadgen", "--set", "numbered", "--cps", "3"]).unwrap();
    assert_eq!(flags.set, "numbered");
    assert_eq!(args.cps, 3.0);
    assert!(args.explicit("cps") && !args.explicit("duration"));
}

#[test]
fn product_defaults_replace_the_loadgen_defaults_but_not_an_explicit_flag() {
    let (_, args) = parse_from(&Other, ["other-loadgen"]).unwrap();
    assert_eq!(args.ring_delay_ms, 2000);
    assert_eq!(args.check_sets_dir, std::path::PathBuf::from("other/checksets"));
    assert!(!args.explicit("ring_delay_ms"), "a product default is not an explicit flag");

    let (_, args) = parse_from(&Other, ["other-loadgen", "--ring-delay-ms", "5"]).unwrap();
    assert_eq!(args.ring_delay_ms, 5);

    let (_, shipped) = parse_from(&Shipped, ["loadgen"]).unwrap();
    assert_eq!(shipped.ring_delay_ms, 0, "the shipped product keeps the loadgen default");
}

#[test]
fn configure_sets_a_mode_default_and_yields_to_an_explicit_flag() {
    let (_, args) = parse_from(&Other, ["other-loadgen", "--set", "numbered"]).unwrap();
    assert_eq!(args.correlate, "to-user");
    let (_, args) =
        parse_from(&Other, ["other-loadgen", "--set", "numbered", "--correlate", "header"])
            .unwrap();
    assert_eq!(args.correlate, "header");
    let (_, args) = parse_from(&Other, ["other-loadgen"]).unwrap();
    assert_eq!(args.correlate, "header");
}

#[test]
fn a_refused_configuration_is_a_usage_error() {
    let err = parse_from(&Other, ["other-loadgen", "--set", "huge"]).map(|_| ()).unwrap_err();
    assert_eq!(err.kind(), clap::error::ErrorKind::ValueValidation);
    assert!(err.to_string().contains("unknown set"), "{err}");
}

#[test]
fn the_registry_follows_the_flags_and_the_resolved_layout() {
    let (flags, _) = parse_from(&Other, ["other-loadgen", "--set", "numbered"]).unwrap();
    let shared = layout("127.0.0.1:6000", "127.0.0.1:6000");
    let split = layout("127.0.0.1:6000", "127.0.0.1:6001");
    let ids =
        |cfg: &EndpointConfig| Other.registry(&flags, &RunLayout { endpoint: cfg }).unwrap().ids();
    assert_eq!(ids(&shared), ["other_numbered_shared"]);
    assert_eq!(ids(&split), ["other_numbered"]);
}

#[test]
fn the_command_carries_the_product_name() {
    let cmd = command(&Other);
    assert_eq!(cmd.get_name(), "other-loadgen");
    assert!(cmd.get_arguments().any(|a| a.get_id() == "set"));
}

struct Misnamed;

impl Product for Misnamed {
    type Flags = NoFlags;
    fn name(&self) -> &'static str {
        "misnamed"
    }
    fn about(&self) -> &'static str {
        "a product whose default names no flag"
    }
    fn flag_defaults(&self) -> &'static [(&'static str, &'static str)] {
        &[("ring_delay", "5")]
    }
    fn registry(&self, _: &NoFlags, _: &RunLayout<'_>) -> Result<ShapeRegistry, String> {
        Ok(ShapeRegistry::empty())
    }
}

#[test]
#[should_panic]
fn a_default_naming_no_flag_fails_at_startup() {
    let _ = command(&Misnamed);
}
