//! The readers in `deploy/` (dashboards, probes, chaos and endurance scripts)
//! spell only names, selector labels and closed label values the catalogues
//! hold.

use std::path::PathBuf;

use metric_contract::allow::Allow;
use metric_contract::contract::Contract;
use metric_contract::scan::check_paths;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

/// The files git tracks under `deploy/`: a run's results or data left on
/// disk is no reader.
fn readers() -> Vec<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root())
        .args(["ls-files", "-z", "--", "deploy"])
        .output()
        .expect("git ls-files");
    assert!(out.status.success(), "git ls-files: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
        .split(|b| *b == 0)
        .filter(|p| !p.is_empty())
        .map(|p| root().join(String::from_utf8_lossy(p).as_ref()))
        .collect()
}

fn allow() -> Allow {
    let file = root().join("deploy/metric-names.allow");
    let mut allow = Allow::default();
    let text = std::fs::read_to_string(&file).expect("allow-list");
    allow.add(&file, &text).expect("allow-list entries");
    allow
}

#[test]
fn the_deploy_readers_hold_to_the_contract() {
    let contract = Contract::from_json(&metric_contract::export()).expect("contract");
    let findings = check_paths(&contract, &allow(), &readers());
    let text: Vec<String> = findings.iter().map(ToString::to_string).collect();
    assert!(text.is_empty(), "{}", text.join("\n"));
}

/// A family a reader names, renamed in the catalogue, is a finding.
#[test]
fn a_renamed_family_its_readers_name_is_a_finding() {
    let mut contract = Contract::from_json(&metric_contract::export()).expect("contract");
    let record = contract.families.remove("b2bua_active_calls").expect("a read family");
    contract.families.insert("b2bua_live_calls".to_owned(), record);
    let findings = check_paths(&contract, &allow(), &readers());
    assert!(findings.iter().any(|f| f.name == "b2bua_active_calls"), "{findings:?}");
}
