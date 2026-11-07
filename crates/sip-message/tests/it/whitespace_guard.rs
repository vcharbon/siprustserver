//! Wire text in this crate is trimmed and split on SP and HTAB alone, through
//! `crate::whitespace`: no source file calls a standard whitespace routine,
//! which also takes VT, FF, CR, LF or, for the Unicode ones, NBSP for
//! whitespace. The listed paths are the ones that read no SIP grammar this
//! way.

use std::path::{Path, PathBuf};

/// The standard whitespace routines, as a call names them.
const ROUTINES: &[&str] = &[
    ".trim()",
    ".trim_start()",
    ".trim_end()",
    "split_whitespace",
    "split_ascii_whitespace",
    "is_ascii_whitespace",
    "is_whitespace",
    "trim_ascii",
];

/// `(path under src/, why it reads no SIP whitespace)`.
const OUTSIDE: &[(&str, &str)] = &[
    ("sniff", "tooling that reads captured text as found"),
    ("mscml.rs", "XML, whose whitespace is XML 1.0 §2.3 `S`"),
    ("multipart", "the multipart framing and part-head readers"),
];

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the source tree reads") {
        let path = entry.expect("an entry").path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_source_calls_a_standard_whitespace_routine() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    let mut found = Vec::new();
    for file in files {
        let relative = file.strip_prefix(&src).expect("under src").to_string_lossy().to_string();
        if OUTSIDE.iter().any(|(path, _)| relative.starts_with(path)) {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("a source reads");
        for (at, line) in text.lines().enumerate() {
            let code = line.split("//").next().unwrap_or_default();
            for routine in ROUTINES.iter().filter(|r| code.contains(*r)) {
                found.push(format!("src/{relative}:{}: {routine}", at + 1));
            }
        }
    }
    assert!(found.is_empty(), "use crate::whitespace instead:\n{}", found.join("\n"));
}
