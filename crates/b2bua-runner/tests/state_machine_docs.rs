//! ADR-0016 slice 4 — committed state-machine diagrams stay fresh, and the
//! composed registry is well-formed. Mirrors the ABNF-corpus freshness model:
//! `docs/sm/*` is committed (so reviewers see each machine's control flow at a
//! glance) and CI fails if it drifts from the generator.

use std::fs;
use std::path::PathBuf;

/// Workspace-root `docs/sm` (relative to this crate).
fn sm_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // crates/
    p.pop(); // workspace root
    p.push("docs/sm");
    p
}

/// Regenerating in memory must match every committed `docs/sm/<machine>.md`
/// byte-for-byte. Run `cargo run -p xtask -- state-machine-docs` to refresh.
#[test]
fn committed_diagrams_are_fresh() {
    let dir = sm_dir();
    for (machine, expected) in b2bua_runner::state_machine_docs() {
        let path = dir.join(format!("{machine}.md"));
        let committed = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!(
                "missing committed diagram {}: {e} — run `cargo run -p xtask -- state-machine-docs`",
                path.display()
            )
        });
        assert_eq!(
            committed, expected,
            "docs/sm/{machine}.md is stale — run `cargo run -p xtask -- state-machine-docs`"
        );
    }
    // The rendered HTML view stays fresh the same way.
    let html_path = dir.join("index.html");
    let committed_html = fs::read_to_string(&html_path).unwrap_or_else(|e| {
        panic!(
            "missing committed {}: {e} — run `cargo run -p xtask -- state-machine-docs`",
            html_path.display()
        )
    });
    assert_eq!(
        committed_html,
        b2bua_runner::state_machine_docs_html(),
        "docs/sm/index.html is stale — run `cargo run -p xtask -- state-machine-docs`"
    );
}

/// Every machine the renderer emits has a committed file, and no orphan diagrams
/// linger for a machine that is no longer registered.
#[test]
fn no_orphan_diagrams() {
    let dir = sm_dir();
    let generated: std::collections::BTreeSet<String> = b2bua_runner::state_machine_docs()
        .into_iter()
        .map(|(m, _)| format!("{m}.md"))
        .collect();
    for entry in fs::read_dir(&dir).expect("docs/sm exists") {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        if name.ends_with(".md") {
            assert!(
                generated.contains(&name),
                "orphan diagram docs/sm/{name} — no registered machine produces it"
            );
        }
    }
}

/// Static validation (ADR-0016 X5): every rule a service contributes belongs to
/// that service's own machine. (Empty registry today; the check guards slices
/// 7/8.)
#[test]
fn composed_registry_is_well_formed() {
    let violations = b2bua::rules::check_registry(&b2bua_runner::compose_services());
    assert!(violations.is_empty(), "registry violations: {violations:?}");
}
