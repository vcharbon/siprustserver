//! The **run bundle**: the record kind one run leaves on disk.
//!
//! ```text
//! <run-dir>/
//!   pivot.json        the document that ran, canonically formatted
//!   run-config.json   the lane's compiled configuration
//!   recording/<leg>.jsonl   every datagram, verbatim, in wire order
//!   verdict.json      what the run decided, and why it failed if it did
//!   timing.json       when it started, when it settled, and the budget
//! ```
//!
//! The writer holds the recording and is armed BEFORE the run body, so a
//! panicking run still leaves its ladder: `write` is called from outside the
//! body's unwind boundary and never needs the run to have finished.
//!
//! This is the WRITING of a bundle; every record kind it lays down is
//! [`pivot_schema::bundle`]'s.

use std::io;
use std::path::{Path, PathBuf};

use pivot_schema::bundle::{Failure, RunConfig, RunTiming, RunVerdict};
use serde::Serialize;

use crate::recording::Recording;

/// One run's bundle, ready to write.
#[derive(Debug, Clone)]
pub struct RunBundle {
    pub document: String,
    pub config: RunConfig,
    pub recording: Recording,
    pub verdict: RunVerdict,
    pub timing: RunTiming,
}

impl RunBundle {
    /// Write the bundle under `dir`, creating it. Every file is written or the
    /// call fails saying which: a partially written bundle is a bundle that
    /// lies about what ran.
    pub fn write(&self, dir: &Path) -> io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        std::fs::write(dir.join("pivot.json"), &self.document)?;
        write_json(&dir.join("run-config.json"), &self.config)?;
        write_json(&dir.join("verdict.json"), &self.verdict)?;
        write_json(&dir.join("timing.json"), &self.timing)?;
        let recording_dir = dir.join("recording");
        std::fs::create_dir_all(&recording_dir)?;
        for (leg, text) in self.recording.to_jsonl() {
            std::fs::write(recording_dir.join(format!("{leg}.jsonl")), text)?;
        }
        Ok(dir.to_path_buf())
    }
}

/// A bundle writer armed BEFORE the run body and written on the way out —
/// including on unwind.
///
/// The run's evidence is the run's product, and a panicking run needs it most:
/// a harness assertion or an RFC gate that fires mid-flow must still leave the
/// ladder on disk. The writer holds the recording handle (which the run mutates
/// through its own clone), so what lands is whatever the run had reached.
pub struct BundleWriter {
    dir: PathBuf,
    document: String,
    config: RunConfig,
    recording: Recording,
    /// Filled by [`finish`](BundleWriter::finish) on a run that returned. A run
    /// that unwound leaves it empty, and the bundle says so rather than
    /// claiming a verdict nothing produced.
    outcome: Option<(RunVerdict, RunTiming)>,
    case: String,
    settle_budget_ms: u64,
    /// Set by [`commit`](BundleWriter::commit): the bundle is already on disk
    /// and the drop write must not run again over it.
    committed: bool,
}

impl BundleWriter {
    /// Arm a writer over `dir`. `recording` is the handle the run records into.
    pub fn arm(
        dir: impl Into<PathBuf>,
        case: impl Into<String>,
        document: String,
        config: RunConfig,
        recording: Recording,
        settle_budget_ms: u64,
    ) -> Self {
        BundleWriter {
            dir: dir.into(),
            document,
            config,
            recording,
            outcome: None,
            case: case.into(),
            settle_budget_ms,
            committed: false,
        }
    }

    /// The recording handle the run records into — a clone of the one the
    /// writer holds, so what the writer lands is whatever the run reached.
    pub fn recording(&self) -> Recording {
        self.recording.clone()
    }

    /// Hand the writer what the run decided. Without this the writer still
    /// writes, under a verdict that states the run never returned.
    pub fn finish(&mut self, verdict: RunVerdict, timing: RunTiming) {
        self.outcome = Some((verdict, timing));
    }

    /// Write the bundle now and surface the result, instead of leaving it to
    /// the drop write that reports only to stderr. The drop write stays armed
    /// for the paths that never reach here — a panic, an early return.
    pub fn commit(&mut self) -> io::Result<PathBuf> {
        self.committed = true;
        self.bundle().write(&self.dir)
    }

    /// The bundle as it stands right now.
    fn bundle(&self) -> RunBundle {
        let (verdict, timing) = self.outcome.clone().unwrap_or_else(|| {
            let mut verdict = RunVerdict::ok(self.case.clone(), self.config.lane.clone());
            verdict.fail(Failure::RunUnwound {
                detail: "the run body did not return; the recording is what it had reached".into(),
            });
            (
                verdict,
                RunTiming {
                    started_at_ms: 0,
                    settled_at_ms: None,
                    settle_budget_ms: self.settle_budget_ms,
                },
            )
        });
        RunBundle {
            document: self.document.clone(),
            config: self.config.clone(),
            recording: self.recording.clone(),
            verdict,
            timing,
        }
    }
}

/// Writing on the way out is the point: a `?`, a panic or an early return all
/// leave the bundle behind.
impl Drop for BundleWriter {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        // A writer that cannot write says so on stderr rather than masking the
        // failure that is already unwinding.
        if let Err(e) = self.bundle().write(&self.dir) {
            eprintln!("[pivot-interpreter] run bundle {} not written: {e}", self.dir.display());
        }
    }
}

/// Every record kind lands in the ONE byte form the contract states
/// (`PCAP2TEST_PIVOT_V3.md` §2.1), the same as `pivot.json` beside it: a reader
/// diffing two bundles diffs their content, never their layout.
fn write_json<T: Serialize>(path: &Path, value: &T) -> io::Result<()> {
    let text = pivot_schema::canonical::format(value)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pivot_schema::bundle::{ClockMode, Dir};

    #[test]
    fn a_bundle_writes_every_file_the_record_kind_names() {
        let recording = Recording::new();
        recording.push("A", Dir::Out, 0, "INVITE sip:x SIP/2.0\r\n\r\n", Some("s1"), None);
        recording.push("B", Dir::In, 10, "INVITE sip:x SIP/2.0\r\n\r\n", Some("s3"), None);
        let bundle = RunBundle {
            document: "{\n  \"pivot_version\": 3\n}\n".into(),
            config: RunConfig::new("upstream-fake", ClockMode::Virtual, "127.0.0.1:5080"),
            recording,
            verdict: RunVerdict::ok("case", "upstream-fake"),
            timing: RunTiming { started_at_ms: 0, settled_at_ms: Some(1420), settle_budget_ms: 32000 },
        };
        let dir = std::env::temp_dir().join(format!("pivot-bundle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        bundle.write(&dir).expect("the bundle writes");
        for file in ["pivot.json", "run-config.json", "verdict.json", "timing.json"] {
            assert!(dir.join(file).is_file(), "{file} missing");
        }
        assert!(dir.join("recording/A.jsonl").is_file());
        assert!(dir.join("recording/B.jsonl").is_file());
        let timing: RunTiming =
            serde_json::from_str(&std::fs::read_to_string(dir.join("timing.json")).unwrap())
                .unwrap();
        assert_eq!(timing.settled_at_ms, Some(1420));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    fn armed(dir: &Path, recording: Recording) -> BundleWriter {
        BundleWriter::arm(
            dir,
            "case",
            "{\n  \"pivot_version\": 3\n}\n".into(),
            RunConfig::new("upstream-fake", ClockMode::Virtual, "127.0.0.1:5080"),
            recording,
            32000,
        )
    }

    #[test]
    fn a_run_that_unwinds_still_leaves_its_ladder_on_disk() {
        let dir = std::env::temp_dir().join(format!("pivot-unwind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let recording = Recording::new();
        let outside = recording.clone();
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _writer = armed(&dir, recording);
            outside.push("A", Dir::Out, 0, "INVITE sip:x SIP/2.0\r\n\r\n", Some("s1"), None);
            panic!("the run body fails mid-flow");
        }));
        assert!(panicked.is_err(), "the panic still propagates");
        assert!(dir.join("recording/A.jsonl").is_file(), "the ladder survived the unwind");
        let verdict: RunVerdict =
            serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap())
                .unwrap();
        assert!(!verdict.passed(), "an unwound run never reads as green");
        assert!(matches!(verdict.failures.first(), Some(Failure::RunUnwound { .. })));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }

    #[test]
    fn a_run_that_returned_writes_the_verdict_it_produced() {
        let dir = std::env::temp_dir().join(format!("pivot-finish-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        {
            let mut writer = armed(&dir, Recording::new());
            writer.finish(
                RunVerdict::ok("case", "upstream-fake"),
                RunTiming { started_at_ms: 0, settled_at_ms: Some(740), settle_budget_ms: 32000 },
            );
        }
        let verdict: RunVerdict =
            serde_json::from_str(&std::fs::read_to_string(dir.join("verdict.json")).unwrap())
                .unwrap();
        assert!(verdict.passed());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
