//! The exported contract: deterministic, read back whole, its names pinned.

use std::path::PathBuf;

use metric_contract::contract::Contract;

fn pinned() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("catalogue.names")
}

/// One line per family of each binary: binary, name, kind, presence, label
/// names per block.
fn names() -> String {
    let mut out = String::new();
    for c in metric_contract::catalogues() {
        for f in c.families() {
            let blocks: Vec<String> = f
                .labels
                .blocks()
                .iter()
                .map(|dims| dims.iter().map(|d| d.name).collect::<Vec<_>>().join(","))
                .collect();
            let presence = match f.cap() {
                Some(cap) => format!("capped:{}", cap.max),
                None if f.is_semi_open() => "semi_open".to_owned(),
                None => "fixed".to_owned(),
            };
            out.push_str(&format!(
                "{} {} {} {} {{{}}}\n",
                c.binary,
                f.name,
                f.kind.as_str(),
                presence,
                blocks.join("|")
            ));
        }
    }
    out
}

/// The export is the same on every run and reads back as a contract holding
/// every family of every catalogue.
#[test]
fn the_export_is_deterministic_and_reads_back_whole() {
    let json = metric_contract::export();
    assert_eq!(json, metric_contract::export());
    let contract = Contract::from_json(&json).expect("the export reads back");
    for c in metric_contract::catalogues() {
        for f in c.families() {
            let record = contract.families.get(f.name).unwrap_or_else(|| panic!("{}", f.name));
            assert_eq!(record.kind, f.kind.as_str(), "{}", f.name);
        }
    }
    for ns in ["b2bua_", "sip_", "limiter_", "loadgen_", "cdr_", "jemalloc_", "trace_", "process_"]
    {
        assert!(contract.namespaces.iter().any(|n| n == ns), "{ns} in {:?}", contract.namespaces);
    }
}

/// Every family's name, kind, presence and labels are pinned in
/// `catalogue.names`: a rename, a new family or a removed one fails here
/// until the file is regenerated (`METRIC_CONTRACT_BLESS=1`) and the change
/// reviewed with the readers it affects.
#[test]
fn the_catalogue_names_are_pinned() {
    let names = names();
    if std::env::var_os("METRIC_CONTRACT_BLESS").is_some() {
        std::fs::write(pinned(), &names).expect("write catalogue.names");
        return;
    }
    let want = std::fs::read_to_string(pinned()).unwrap_or_default();
    if names != want {
        let got: Vec<&str> = names.lines().collect();
        let had: Vec<&str> = want.lines().collect();
        let added: Vec<&&str> = got.iter().filter(|l| !had.contains(l)).collect();
        let removed: Vec<&&str> = had.iter().filter(|l| !got.contains(l)).collect();
        panic!(
            "the catalogues changed against catalogue.names (rerun with METRIC_CONTRACT_BLESS=1 \
             once the readers follow):\n  added: {added:#?}\n  removed: {removed:#?}"
        );
    }
}

/// Every cap of every catalogued family names labels of its family: a
/// misspelt one would bound nothing.
#[test]
fn every_cap_names_labels_of_its_family() {
    for c in metric_contract::catalogues() {
        for f in c.families() {
            let Some(cap) = f.cap() else { continue };
            let blocks = f.labels.blocks();
            for label in cap.labels {
                let named = blocks.iter().any(|dims| dims.iter().any(|d| d.name == *label));
                assert!(named, "{}: the cap names no label {label:?}", f.name);
            }
        }
    }
}
