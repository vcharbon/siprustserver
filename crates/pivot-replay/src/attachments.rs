//! A cell's attachments (ADR-0060 appendix §9): every file the replay
//! leaves in the cell's bundle for the site to show, listed in
//! `attachments.jsonl` with its kind, one `@sip/contracts` `Attachment` line
//! appended each time the file is written. The last line naming an attachment
//! states it, so a file written twice is listed once.

use std::io::Write;
use std::path::Path;

use serde::Serialize;

use crate::Failure;

/// The bundle file listing a cell's attachments.
pub const FILE: &str = "attachments.jsonl";

/// Who attaches the replay's files.
pub const SOURCE: &str = "replay";

/// How the site renders an attachment's file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Log,
    Records,
    Diff,
    Table,
    Json,
    File,
    Link,
}

/// One file of the cell as the list states it: unique by `name` within the
/// cell, `file` relative to the cell's bundle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Attachment {
    pub name: &'static str,
    pub kind: Kind,
    pub title: &'static str,
    pub file: &'static str,
}

#[derive(Serialize)]
struct Line<'a> {
    #[serde(flatten)]
    attachment: &'a Attachment,
    source: &'static str,
}

/// Appends `attachment` to the list of the cell whose bundle is `out_dir`.
pub fn attach(out_dir: &Path, attachment: &Attachment) -> Result<(), Failure> {
    let line = serde_json::to_string(&Line { attachment, source: SOURCE })
        .map_err(|e| Failure::Environment(format!("serialize {FILE}: {e}")))?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(out_dir.join(FILE))
        .and_then(|mut file| file.write_all(format!("{line}\n").as_bytes()))
        .map_err(|e| Failure::Environment(format!("write {FILE}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: Attachment =
        Attachment { name: "sut-log", kind: Kind::Log, title: "the SUT's log", file: "sut.log" };

    #[test]
    fn appends_one_contract_line_per_write() {
        let dir =
            std::env::temp_dir().join(format!("pivot-replay-attachments-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        attach(&dir, &LOG).unwrap();
        attach(&dir, &Attachment { file: "sut-2.log", ..LOG }).unwrap();
        let text = std::fs::read_to_string(dir.join(FILE)).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let lines: Vec<serde_json::Value> =
            text.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        assert_eq!(
            lines,
            vec![
                serde_json::json!({"name": "sut-log", "kind": "log", "title": "the SUT's log", "file": "sut.log", "source": "replay"}),
                serde_json::json!({"name": "sut-log", "kind": "log", "title": "the SUT's log", "file": "sut-2.log", "source": "replay"}),
            ]
        );
    }
}
