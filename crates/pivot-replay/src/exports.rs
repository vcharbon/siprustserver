//! Exports and keys: what a cell's or a batch's own process leaves under a
//! collector's name when it ends, through the registration seam
//! (ADR-0060 appendix §4, §13). An export is `exports/<collector>.jsonl` in the
//! cell's or the batch's directory, one line per record or key, in the line
//! forms a collector's `collect` answers (appendix §5), a cell named by its
//! directory's name (appendix §6). This module knows no collector: a
//! registration names it and builds its lines.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::Failure;

/// The directory of a cell's or a batch's exports.
pub const DIR: &str = "exports";

/// One line of an export.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Line {
    /// A record the base joins to the cells whose key the collector's
    /// `[join] key` names holds `key`.
    Keyed { record: serde_json::Value, key: String },
    /// A record the exporter assigned to a cell itself, `cell` naming the
    /// cell's directory, which the base reads as that cell.
    Assigned { record: serde_json::Value, cell: String },
    /// The key named `key` added to the cell whose directory `cell` names,
    /// with its `values`.
    Key { key: String, cell: String, values: Vec<String> },
}

/// Where the export of `collector` lives under the cell's or the batch's
/// directory `dir`.
pub fn path_of(dir: &Path, collector: &str) -> PathBuf {
    dir.join(DIR).join(format!("{collector}.jsonl"))
}

/// Writes `lines` as the export of `collector` under `dir`, one JSON object
/// per line, replacing any earlier one: a scope ends once.
pub fn write(dir: &Path, collector: &str, lines: &[Line]) -> Result<PathBuf, Failure> {
    let path = path_of(dir, collector);
    let unwritable = |e: std::io::Error| Failure::Environment(format!("{}: {e}", path.display()));
    std::fs::create_dir_all(dir.join(DIR)).map_err(unwritable)?;
    let mut text = String::new();
    for line in lines {
        text.push_str(&serde_json::to_string(line).expect("an export line serializes"));
        text.push('\n');
    }
    std::fs::write(&path, text).map_err(unwritable)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Each line takes its appendix form, and the file holds one per line
    /// under `exports/<collector>.jsonl`, a second write replacing the first.
    #[test]
    fn an_export_is_one_line_per_record_or_key_under_its_collector_name() {
        let dir = std::env::temp_dir().join(format!("pivot-replay-exports-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let lines = [
            Line::Keyed { record: json!({"n": 1}), key: "id-1".into() },
            Line::Assigned { record: json!({"n": 2}), cell: "c1".into() },
            Line::Key { key: "ref".into(), cell: "c1".into(), values: vec!["r1".into()] },
        ];
        write(&dir, "records", &[Line::Keyed { record: json!({}), key: "old".into() }])
            .expect("written");
        let path = write(&dir, "records", &lines).expect("written");
        assert_eq!(path, dir.join("exports/records.jsonl"));
        let read: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .expect("read")
            .lines()
            .map(|line| serde_json::from_str(line).expect("a JSON line"))
            .collect();
        assert_eq!(
            read,
            [
                json!({"record": {"n": 1}, "key": "id-1"}),
                json!({"record": {"n": 2}, "cell": "c1"}),
                json!({"key": "ref", "cell": "c1", "values": ["r1"]}),
            ]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
