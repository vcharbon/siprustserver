//! Workspace automation. Run via `cargo run -p xtask -- <task>`.
//!
//! Tasks:
//!   abnf-regen [N]   Regenerate the frozen ABNF corpus from the vendored
//!                    grammars using `abnfgen` (external binary; install per
//!                    crates/sip-message/tests/abnf/README.md). N samples per
//!                    target, default 1000. Writes tests/abnf/corpus/<t>.txt.
//!
//! The corpus is committed so CI is deterministic and needs no external
//! binary at test time; this task is the opt-in refresh path. See
//! docs/MIGRATION_STRATEGY.md § ABNF tests.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

/// (corpus target file stem, abnfgen start symbol). Mirrors the source
/// `scripts/abnf-fuzz/run-suite.sh` TARGETS table.
const TARGETS: &[(&str, &str)] = &[
    ("sip-uri", "SIP-URI"),
    ("from", "from-spec"),
    ("pai", "pai-header-value"),
    ("contact", "contact-value"),
    ("via", "via-header-value"),
    ("cseq", "cseq-value"),
    ("rack", "rack-value"),
    ("replaces", "replaces-value"),
    ("refer-to", "refer-to-value"),
    ("request-line", "request-line"),
];

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("abnf-regen") => {
            let n: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(1000);
            match abnf_regen(n) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("abnf-regen failed: {e}");
                    ExitCode::FAILURE
                }
            }
        }
        other => {
            eprintln!("unknown task: {other:?}\nusage: cargo run -p xtask -- abnf-regen [N]");
            ExitCode::FAILURE
        }
    }
}

/// `crates/sip-message/tests/abnf` relative to this xtask crate.
fn abnf_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.pop(); // workspace root
    p.push("crates/sip-message/tests/abnf");
    p
}

fn abnf_regen(n: usize) -> Result<(), String> {
    let abnfgen = std::env::var("ABNFGEN").unwrap_or_else(|_| "abnfgen".to_string());
    // Fail fast with a helpful message if the binary is absent.
    if Command::new(&abnfgen).arg("-h").output().is_err() {
        return Err(format!(
            "`{abnfgen}` not found. Install it (https://www.quut.com/abnfgen/) and/or set \
             $ABNFGEN. See crates/sip-message/tests/abnf/README.md."
        ));
    }

    let base = abnf_dir();
    let grammars = base.join("grammars");
    let corpus = base.join("corpus");
    fs::create_dir_all(&corpus).map_err(|e| format!("create corpus dir: {e}"))?;

    let common = fs::read_to_string(grammars.join("_common.abnf"))
        .map_err(|e| format!("read _common.abnf: {e}"))?;

    let tmp_root = std::env::temp_dir().join("abnf-regen");
    fs::create_dir_all(&tmp_root).map_err(|e| format!("create tmp dir: {e}"))?;

    for (target, start) in TARGETS {
        // Concatenate _common.abnf with the per-target grammar (abnfgen takes a
        // single grammar file).
        let per_target = fs::read_to_string(grammars.join(format!("{target}.abnf")))
            .map_err(|e| format!("read {target}.abnf: {e}"))?;
        let merged_path = tmp_root.join(format!("{target}.abnf"));
        fs::write(&merged_path, format!("{common}\n{per_target}"))
            .map_err(|e| format!("write merged grammar: {e}"))?;

        let mut samples = String::new();
        for seed in 1..=n {
            let out = Command::new(&abnfgen)
                .arg("-s")
                .arg(start)
                .arg("-r")
                .arg(seed.to_string())
                .arg(&merged_path)
                .output()
                .map_err(|e| format!("run abnfgen for {target}: {e}"))?;
            // Each run yields one sample; flatten any internal newlines so the
            // corpus stays one-sample-per-line (the test splits on \n).
            let line = String::from_utf8_lossy(&out.stdout).replace(['\r', '\n'], "");
            samples.push_str(&line);
            samples.push('\n');
        }

        let out_path = corpus.join(format!("{target}.txt"));
        write_corpus(&out_path, &samples)?;
        eprintln!("abnf-regen: {target:<14} {n} samples -> {}", rel(&out_path, &base));
    }

    Ok(())
}

fn write_corpus(path: &Path, contents: &str) -> Result<(), String> {
    fs::write(path, contents).map_err(|e| format!("write {}: {e}", path.display()))
}

fn rel(path: &Path, base: &Path) -> String {
    path.strip_prefix(base).unwrap_or(path).display().to_string()
}
