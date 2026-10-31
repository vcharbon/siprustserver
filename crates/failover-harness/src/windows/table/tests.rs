use std::collections::BTreeSet;

use super::*;

/// The rows no upstream sweep cell fires today, at either level: kept, as
/// Degraded keeps every §11 window, until a shape or incident exercises them.
const UNWITNESSED: &[&str] = &[
    "W-cdr-split",
    "W-kill-cancel-answered",
    "W-kill-non-2xx-after-2xx",
    "W-kill-teardown-lost",
    "W-off-answer-lost",
    "W-off-reboot-two-servers",
    "W-split-early-bye",
    "W-stale-resend-cseq",
    "W-stale-resend-cseq-reuse",
    "W-stale-resend-reinvite",
    "W-stall-cancel-reason",
];

/// The backticked `W-…` ids of ADR-0055 §11.
fn adr_ids() -> BTreeSet<String> {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/adr/0055-services-are-ha-blind.md");
    let adr = std::fs::read_to_string(path).expect("ADR-0055 reads");
    let start = adr.find("### 11. The proof").expect("ADR-0055 has §11");
    let end = adr[start..].find("\n### 12.").map_or(adr.len(), |e| start + e);
    adr[start..end]
        .split('`')
        .skip(1)
        .step_by(2)
        .filter(|s| s.starts_with("W-"))
        .map(str::to_string)
        .collect()
}

#[test]
fn every_section_11_bullet_and_every_row_share_one_id() {
    let rows: BTreeSet<String> =
        TABLE.iter().filter(|r| r.id.starts_with("W-")).map(|r| r.id.to_string()).collect();
    let adr = adr_ids();
    assert!(UNWITNESSED.iter().all(|id| rows.contains(*id)), "{UNWITNESSED:?}");
    let unstated: Vec<_> = rows.difference(&adr).collect();
    let rowless: Vec<_> = adr.difference(&rows).collect();
    assert!(unstated.is_empty(), "rows ADR-0055 §11 does not state: {unstated:?}");
    assert!(rowless.is_empty(), "ADR-0055 §11 ids no row carries: {rowless:?}");
}

#[test]
fn ids_are_unique_and_no_row_opens_in_the_guaranteed_tier() {
    let mut seen = BTreeSet::new();
    for row in TABLE {
        assert!(seen.insert(row.id), "{} is twice in the table", row.id);
        assert!(row.id.starts_with("W-") || row.id.starts_with("T-"), "{}", row.id);
        assert_ne!(row.tier, Tier::Guaranteed, "{} opens in the Guaranteed tier", row.id);
    }
}

#[test]
fn every_witness_is_a_shape_of_the_upstream_catalogue() {
    let sweep = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/it/sweep.rs");
    let sweep = std::fs::read_to_string(sweep).expect("the sweep's test file reads");
    for row in TABLE {
        let Some(w) = row.witness else { continue };
        let module = w.shape.replace('-', "_");
        assert!(
            sweep.contains(&format!("    {module} => ")),
            "{}'s witness shape {} is not in tests/it/sweep.rs",
            row.id,
            w.shape
        );
    }
}

#[test]
fn every_row_but_a_transitional_or_unwitnessed_one_names_a_witness_its_incidents_strike() {
    for row in TABLE {
        let unwitnessed = row.id.starts_with("T-") || UNWITNESSED.contains(&row.id);
        match (unwitnessed, row.witness) {
            (true, None) => {}
            (true, Some(_)) => panic!("{} names a witness: off the unwitnessed list", row.id),
            (false, None) => panic!("{} names no witness cell", row.id),
            (false, Some(w)) => assert!(
                row.opens.incidents().any(|i| i == w.incident),
                "{}'s witness is cut by {:?}, which does not open it",
                row.id,
                w.incident
            ),
        }
    }
}
