//! The readers in `deploy/` (the lab scripts, the SIPp exporter), the base
//! dashboards of the HA tooling and its runner's own queries
//! (`tooling/hakit/src`) spell only names, selector labels and
//! closed label values the catalogues hold: the shipped binaries' export
//! extended with the base content's own catalogues.

use std::path::PathBuf;

use metric_contract::allow::Allow;
use metric_contract::contract::Contract;
use metric_contract::scan::check_paths;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

fn base_content() -> PathBuf {
    root().join("tooling/hakit/content")
}

/// The files git tracks under `deploy/`, the base dashboards and the
/// runner's source: a run's results or data left on disk is no reader.
fn readers() -> Vec<PathBuf> {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(root())
        .args([
            "ls-files",
            "-z",
            "--",
            "deploy",
            "tooling/hakit/content/dashboards",
            "tooling/hakit/src",
        ])
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

fn contract() -> Contract {
    metric_contract::content::contract(&metric_contract::export(), &[base_content()])
        .expect("contract")
}

#[test]
fn the_deploy_readers_hold_to_the_contract() {
    let findings = check_paths(&contract(), &allow(), &readers());
    let text: Vec<String> = findings.iter().map(ToString::to_string).collect();
    assert!(text.is_empty(), "{}", text.join("\n"));
}

/// The HA tooling's own queries (the lab's CDR and leftover-call queries,
/// the readers' specs) are readers too: a family they name, renamed, is a
/// finding there.
#[test]
fn a_renamed_family_the_tooling_queries_is_a_finding_in_the_tooling() {
    let mut contract = contract();
    let record = contract.families.remove("b2bua_cdr_written_total").expect("a read family");
    contract.families.insert("b2bua_cdr_emitted_total".to_owned(), record);
    let findings = check_paths(&contract, &allow(), &readers());
    let hakit_src = root().join("tooling/hakit/src");
    assert!(
        findings
            .iter()
            .any(|f| f.name == "b2bua_cdr_written_total" && f.file.starts_with(&hakit_src)),
        "{findings:?}"
    );
}

/// A family a reader names, renamed in the catalogue, is a finding.
#[test]
fn a_renamed_family_its_readers_name_is_a_finding() {
    let mut contract = contract();
    let record = contract.families.remove("b2bua_active_calls").expect("a read family");
    contract.families.insert("b2bua_live_calls".to_owned(), record);
    let findings = check_paths(&contract, &allow(), &readers());
    assert!(findings.iter().any(|f| f.name == "b2bua_active_calls"), "{findings:?}");
}

/// The series the tooling pushes are declared by the base content, not
/// allow-listed: without its catalogue, a dashboard reading them fails.
#[test]
fn the_tooling_series_come_from_the_base_content_catalogue() {
    let bare = Contract::from_json(&metric_contract::export()).expect("contract");
    let findings = check_paths(&bare, &allow(), &readers());
    assert!(findings.iter().any(|f| f.name == "sip_chaos_window_verdict"), "{findings:?}");
}
