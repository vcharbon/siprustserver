//! The loop and recursion sites of crates (ADR-0057 §5), for the panic-sites
//! ratchet.
//!
//!   site-scan < request.json
//!
//! The request names the scan root and, per crate, its target roots
//! (relative to the root) and the sets it is counted in:
//!
//!   {"root": "/abs", "crates": [{"name": "a", "roots": ["crates/a/src/lib.rs"],
//!                                "sets": ["loop", "recursion"]}]}
//!
//! Each site is one JSON line on stdout: `reason` `site-scan`, `crate`, `set`,
//! `file_name`, `line_start`, `line_end`, `column_start`, `column_end` and
//! `message`. A file that cannot be read or parsed is named on stderr and
//! the exit code is 1; a malformed request exits 2.

mod cfg;
mod cycles;
mod graph;
mod loops;
mod macros;
mod modules;
mod scope;
mod site;

use std::collections::BTreeSet;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use serde_json::Value;

use crate::scope::Scope;
use crate::site::{Set, Site};

fn main() -> ExitCode {
    let mut text = String::new();
    if let Err(e) = io::stdin().read_to_string(&mut text) {
        eprintln!("site-scan: stdin: {e}");
        return ExitCode::from(2);
    }
    let request: Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("site-scan: the request: {e}");
            return ExitCode::from(2);
        }
    };
    let Some(root) = request.get("root").and_then(Value::as_str) else {
        eprintln!("site-scan: the request names no `root`");
        return ExitCode::from(2);
    };
    let crates = request.get("crates").and_then(Value::as_array).cloned().unwrap_or_default();
    let (mut problems, mut out) = (Vec::new(), io::stdout().lock());
    for c in crates {
        let name = c.get("name").and_then(Value::as_str).unwrap_or_default();
        let strings = |key: &str| -> Vec<String> {
            c.get(key)
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default()
        };
        let sets: BTreeSet<Set> = strings("sets")
            .iter()
            .filter_map(|s| match s.as_str() {
                "loop" => Some(Set::Loop),
                "recursion" => Some(Set::Recursion),
                _ => None,
            })
            .collect();
        let (sites, mut why) = scan(Path::new(root), &strings("roots"), &sets);
        problems.append(&mut why);
        for site in sites {
            if writeln!(out, "{}", site.json(name)).is_err() {
                return ExitCode::from(2);
            }
        }
    }
    for p in &problems {
        eprintln!("site-scan: {p}");
    }
    if problems.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

/// The sites of the targets rooted at `roots`, in `sets`.
pub fn scan(root: &Path, roots: &[String], sets: &BTreeSet<Set>) -> (BTreeSet<Site>, Vec<String>) {
    let (mut sites, mut problems) = (BTreeSet::new(), Vec::new());
    for target in roots {
        let (modules, mut why) = modules::load(root, target);
        problems.append(&mut why);
        if sets.contains(&Set::Loop) {
            for m in &modules {
                let mut scope = Scope::default();
                m.items.iter().for_each(|i| scope.add(i));
                sites.extend(loops::sites(m, &scope));
            }
        }
        if sets.contains(&Set::Recursion) {
            sites.extend(cycles::sites(&graph::build(&modules)));
        }
    }
    (sites, problems)
}

#[cfg(test)]
mod tests;
