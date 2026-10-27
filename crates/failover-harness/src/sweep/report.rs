//! What one sweep test's cells measured, written as JSON to
//! `target/sweep-report/<test>.json`: each cell's tier, the rows that fired
//! in it, whether the evidence rule ended its call, and whether it passed,
//! with the first line of its failure.
//! `just sweep-report` adds every test's file together into counts per row
//! and per tier; nothing gates on them.

use std::path::{Path, PathBuf};

use serde_json::json;

use super::Ran;

/// The directory the tests write to: `$CARGO_TARGET_DIR/sweep-report`, else
/// the workspace's `target/sweep-report`.
pub fn dir() -> PathBuf {
    match std::env::var("CARGO_TARGET_DIR") {
        Ok(t) => PathBuf::from(t).join("sweep-report"),
        Err(_) => Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/sweep-report"),
    }
}

/// Write the cells `ran` of the test whose cells are named `prefix…`.
/// Best effort: a report that cannot be written is said on stderr.
pub fn write(prefix: &str, ran: &[Ran]) {
    let cells: Vec<_> = ran
        .iter()
        .map(|r| {
            json!({
                "cell": r.name,
                "tier": r.measured.tier.map(|t| t.label()),
                "fired": r.measured.fired,
                "evidence": r.measured.evidence,
                "passed": r.failure.is_none(),
                "failure": r.failure.as_deref().and_then(|f| f.lines().find(|l| !l.trim().is_empty())),
            })
        })
        .collect();
    let doc = json!({ "test": prefix, "cells": cells });
    let path = dir().join(format!("{}.json", prefix.trim_end_matches('/').replace('/', "__")));
    let written =
        std::fs::create_dir_all(dir()).and_then(|()| std::fs::write(&path, format!("{doc:#}\n")));
    if let Err(e) = written {
        eprintln!("{prefix}: the sweep report {} is not written: {e}", path.display());
    }
}
