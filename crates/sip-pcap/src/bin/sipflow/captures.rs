//! The capture files a command line names: a named file as given, a
//! directory's capture files oldest first.

use std::path::PathBuf;

/// Every capture file of `inputs`, in the order they are read.
pub(crate) fn expand_inputs(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    for p in inputs {
        if p.is_dir() {
            let mut in_dir: Vec<PathBuf> = std::fs::read_dir(p)
                .map(|rd| {
                    rd.filter_map(|e| e.ok().map(|e| e.path()))
                        .filter(|p| {
                            // Name-based, and deliberately loose: `.pcap`,
                            // `.pcapng` and either gzipped. The container and
                            // compression are re-detected from the bytes.
                            p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.contains(".pcap"))
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Ring files: oldest-first by mtime so fragment reassembly and
            // ladders see time monotonically.
            in_dir.sort_by_key(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok());
            files.extend(in_dir);
        } else {
            files.push(p.clone());
        }
    }
    files
}

#[cfg(all(test, unix))]
#[path = "captures/tests.rs"]
mod tests;
