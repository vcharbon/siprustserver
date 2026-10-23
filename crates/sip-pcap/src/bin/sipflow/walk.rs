//! The census input walk: which flows documents a set of paths names, and the
//! capture each one is counted under.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};

/// Every flows document under `inputs`: a named file is taken as given, a
/// directory is walked recursively for `*.flows.json`.
///
/// The walk follows symlinks, to directories and to documents alike, so a
/// corpus assembled from links is swept whole. It walks the real tree of every
/// input first and the links after, each directory once by its real path, so a
/// document is presented ONCE, under the path that reaches it through no link
/// where one does: a corpus that also classifies its captures by symlinking
/// them into bucket directories would otherwise inflate every count the census
/// reports. A link cycle ends at a directory already walked.
///
/// Sorted, so the sweep's work order — and therefore its report — does not
/// depend on the filesystem.
pub(crate) fn expand_flows_documents(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut walk = Walk::default();
    for input in inputs {
        if input.is_dir() {
            walk.directory(input);
        } else {
            walk.document(input.clone());
        }
    }
    while let Some(link) = walk.links.pop_front() {
        match std::fs::metadata(&link) {
            Ok(target) if target.is_dir() => walk.directory(&link),
            Ok(target) if target.is_file() && is_document(&link) => walk.document(link),
            // A dangling link, or one to anything else, names no document.
            _ => {}
        }
    }
    walk.found.sort();
    walk.found
}

/// The capture a document is counted under: the name of the directory the
/// walk reached it in, which is the name the corpus gives the capture whether
/// that directory is real or a link.
pub(crate) fn capture_of(document: &Path) -> String {
    document
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[derive(Default)]
struct Walk {
    /// The real path of every directory walked.
    walked: HashSet<PathBuf>,
    /// The real path of every document found.
    documents: HashSet<PathBuf>,
    /// Links met and not yet followed, in the order met.
    links: VecDeque<PathBuf>,
    found: Vec<PathBuf>,
}

impl Walk {
    fn directory(&mut self, dir: &Path) {
        let real = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        if !self.walked.insert(real) {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(kind) = entry.file_type() else { continue };
            let path = entry.path();
            if kind.is_symlink() {
                self.links.push_back(path);
            } else if kind.is_dir() {
                self.directory(&path);
            } else if kind.is_file() && is_document(&path) {
                self.document(path);
            }
        }
    }

    fn document(&mut self, path: PathBuf) {
        let real = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if self.documents.insert(real) {
            self.found.push(path);
        }
    }
}

fn is_document(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".flows.json"))
}
