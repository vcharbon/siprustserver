//! The census input walk: which flows documents a set of paths names.

use std::path::{Path, PathBuf};

/// Every flows document under `inputs`: a named file is taken as given, a
/// directory is walked recursively for `*.flows.json`.
///
/// The walk follows symlinks, to directories and to documents alike, so a
/// corpus assembled from links is swept whole. Each document is presented ONCE,
/// by canonical path, and under the path that reaches it through no link where
/// one does: a corpus that also classifies its captures by symlinking them into
/// bucket directories would otherwise inflate every count the census reports.
/// A link back to a directory the walk is already inside is not descended, so
/// a link cycle ends.
///
/// Sorted, so the sweep's work order — and therefore its report — does not
/// depend on the filesystem.
pub(crate) fn expand_flows_documents(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut state = Walk { inside: Vec::new(), found: Vec::new() };
    for input in inputs {
        if input.is_dir() {
            walk(input, false, &mut state);
        } else {
            state.found.push((false, input.clone()));
        }
    }
    // Paths through no link first, so the dedup keeps the real one.
    state.found.sort();
    let mut seen = std::collections::HashSet::new();
    let mut files: Vec<PathBuf> = state
        .found
        .into_iter()
        .filter(|(_, p)| seen.insert(std::fs::canonicalize(p).unwrap_or_else(|_| p.clone())))
        .map(|(_, p)| p)
        .collect();
    files.sort();
    files
}

struct Walk {
    /// The canonical directories from the input down to the one being read.
    inside: Vec<PathBuf>,
    /// Each document found, and whether its path crossed a symlink.
    found: Vec<(bool, PathBuf)>,
}

fn is_document(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".flows.json"))
}

fn walk(dir: &Path, via_link: bool, state: &mut Walk) {
    let real = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if state.inside.contains(&real) {
        return;
    }
    state.inside.push(real);
    read(dir, via_link, state);
    state.inside.pop();
}

fn read(dir: &Path, via_link: bool, state: &mut Walk) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.filter_map(Result::ok) {
        let Ok(kind) = entry.file_type() else { continue };
        let path = entry.path();
        let link = kind.is_symlink();
        // A link is judged by what it points at; a dangling one is skipped.
        let Ok(target) = (if link { std::fs::metadata(&path) } else { entry.metadata() }) else {
            continue;
        };
        if target.is_dir() {
            walk(&path, via_link || link, state);
        } else if target.is_file() && is_document(&path) {
            state.found.push((via_link || link, path));
        }
    }
}
