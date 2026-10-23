//! `docs/B2BUA_RFC_CORNER_CASE_COVERAGE.md` names tests that exist: every
//! `` `name` `` a covered (✅) or partial (🟡) row's "Where / suggested test"
//! cell claims is a test function under `crates/`, or an RFC rule token.
//!
//! The cell's forms: `` `fn` ``, `` <crate> `fn` `` (searched in that crate),
//! `` <file stem> `suffix` `` (the function `<stem>_<suffix>`),
//! `` `stem::fn` `` (searched in `stem.rs`), a `*` or `...` glob over the
//! identifiers in scope, and a kebab-case rule token (`rfc_rules::RuleId`). A
//! name in a clause that says "add" is a proposal and is not checked; a
//! backticked literal that is neither (a header name) is not a reference.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// One source file: its path, its function names and its identifiers.
struct Source {
    path: PathBuf,
    fns: HashSet<String>,
    idents: HashSet<String>,
}

fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn read_sources(dir: &Path, out: &mut Vec<Source>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n != "target") {
                read_sources(&path, out);
            }
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let idents: HashSet<String> = text
                .split(|c: char| !is_ident_char(c))
                .filter(|w| !w.is_empty())
                .map(str::to_string)
                .collect();
            let words: Vec<&str> =
                text.split(|c: char| !is_ident_char(c)).filter(|w| !w.is_empty()).collect();
            let fns = words.windows(2).filter(|w| w[0] == "fn").map(|w| w[1].to_string()).collect();
            out.push(Source { path, fns, idents });
        }
    }
}

/// Whether `glob` (`*` = any identifier characters) matches all of `ident`.
fn glob_matches(glob: &str, ident: &str) -> bool {
    let parts: Vec<&str> = glob.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !ident.starts_with(first) || ident.len() < first.len() + last.len() {
        return false;
    }
    if parts.len() == 1 {
        return ident == glob;
    }
    let mut rest = &ident[first.len()..ident.len() - last.len()];
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    ident.ends_with(last)
}

/// The kebab-case rule token form: lowercase words joined by `-`.
fn is_rule_token(name: &str) -> bool {
    name.contains('-')
        && name.split('-').all(|w| {
            !w.is_empty() && w.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        })
}

/// The bare word right before a backtick, when only spaces separate them.
fn word_before(clause: &str) -> Option<&str> {
    let trimmed = clause.trim_end();
    if trimmed.len() == clause.len() {
        return None;
    }
    let word = trimmed.rsplit(|c: char| c.is_whitespace()).next()?;
    (!word.is_empty() && word.chars().all(|c| is_ident_char(c) || c == '-')).then_some(word)
}

#[test]
fn every_covered_row_names_a_test_that_exists() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let crates = root.join("crates");
    let mut sources = Vec::new();
    read_sources(&crates, &mut sources);
    let tokens: HashSet<&str> = rfc_rules::RuleId::ALL.iter().map(|r| r.token()).collect();
    let doc = std::fs::read_to_string(root.join("docs/B2BUA_RFC_CORNER_CASE_COVERAGE.md"))
        .expect("the coverage doc reads");

    let mut stale = BTreeSet::new();
    let mut checked = 0;
    for line in doc.lines().filter(|l| l.starts_with('|')) {
        let cells: Vec<&str> = line.trim().trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() != 7 || !(cells[5].contains('✅') || cells[5].contains('🟡')) {
            continue;
        }
        let (id, cell) = (cells[0], cells[6]);
        for (start, piece) in cell.match_indices('`').step_by(2) {
            let rest = &cell[start + piece.len()..];
            let Some(end) = rest.find('`') else { break };
            let name = &rest[..end];
            let clause = cell[..start].rsplit(';').next().unwrap_or_default();
            if clause.split(|c: char| !c.is_ascii_alphabetic()).any(|w| w == "add") {
                continue;
            }
            if is_rule_token(name) {
                checked += 1;
                if !tokens.contains(name) {
                    stale.insert(format!("{id}: `{name}` is no rule token"));
                }
                continue;
            }
            if !name.chars().all(|c| is_ident_char(c) || matches!(c, ':' | '*' | '.')) {
                continue;
            }
            checked += 1;
            let (stem, fn_name) = match name.rsplit_once("::") {
                Some((path, f)) => (path.rsplit("::").next(), f),
                None => (None, name),
            };
            let word = word_before(clause);
            let scope: Vec<&Source> = sources
                .iter()
                .filter(|s| match (stem, word) {
                    (Some(stem), _) => {
                        s.path.file_stem().is_some_and(|f| f == stem)
                            || (s.path.file_stem().is_some_and(|f| f == "mod")
                                && s.path
                                    .parent()
                                    .and_then(Path::file_name)
                                    .is_some_and(|d| d == stem))
                    }
                    (None, Some(w)) if crates.join(w).is_dir() => {
                        s.path.starts_with(crates.join(w))
                    }
                    _ => true,
                })
                .collect();
            let glob = fn_name.replace("...", "*");
            let found = if glob.contains('*') {
                scope.iter().any(|s| s.idents.iter().any(|i| glob_matches(&glob, i)))
            } else {
                let prefixed = word.map(|w| format!("{}_{fn_name}", w.replace('-', "_")));
                scope.iter().any(|s| s.fns.contains(fn_name))
                    || prefixed.is_some_and(|p| sources.iter().any(|s| s.fns.contains(&p)))
            };
            if !found {
                stale.insert(format!("{id}: `{name}` names no test"));
            }
        }
    }
    assert!(checked > 100, "the doc's covered rows were read ({checked} names)");
    assert!(
        stale.is_empty(),
        "coverage rows name tests that do not exist (point the row at the real test, or mark \
         it a gap):\n  {}",
        stale.into_iter().collect::<Vec<_>>().join("\n  ")
    );
}
