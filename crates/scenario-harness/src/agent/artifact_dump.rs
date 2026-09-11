//! The Drop-time report-artifact writer: renders the full SVG / HTML / text
//! ladders for a run whose harness never reached
//! [`finish`](super::Harness::finish) — the failing runs, exactly the ones a
//! reader needs a diagram for. Gated by the `SCENARIO_ARTIFACT_DIR` env var,
//! read at Drop time (unset ⇒ off, so CI stays artifact-free unless asked).
//!
//! The guard fires on BOTH a panic unwind and a clean drop without `finish()`;
//! `finish` / `finish_collecting` disarm it. On the panic path it pushes a
//! failed [`ExpectOutcome`] carrying the panic message captured by
//! [`super::panic_note`], so the rendered banner reads FAIL and states why.
//! Rendering reads only synchronous snapshots (recorder + channel) — the
//! recording layer's `close()` never runs here, so the report's `audit` is
//! `Ok(())` and the layer-close structural anomalies are absent. Best-effort
//! and panic-safe: every failure is swallowed after an stderr note (a Drop
//! must never double-panic). The compact stderr trace stays with
//! [`super::run_guards::PanicDump`], which drops first.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use layer_harness::{Channel, Recorder};
use sip_net::SignalingNetworkEvent;

use crate::anchors::AnchorTag;
use crate::run::{ExpectOutcome, RunReport};

/// The env var naming the artifact output root. Unset ⇒ the guard writes
/// nothing.
pub(super) const ARTIFACT_DIR_ENV: &str = "SCENARIO_ARTIFACT_DIR";

/// RAII artifact writer armed for the lifetime of a
/// [`Harness`](super::Harness); see the module doc for the full contract.
pub(super) struct ArtifactDump {
    name: String,
    /// Shared with the owning `Harness` (one source of truth —
    /// [`describe`](super::Harness::describe) writes through the same cell).
    description: Rc<RefCell<Option<String>>>,
    channel: Channel<SignalingNetworkEvent>,
    recorder: Recorder,
    anchors: Rc<RefCell<Vec<AnchorTag>>>,
    armed: Cell<bool>,
}

impl ArtifactDump {
    pub(super) fn new(
        name: String,
        description: Rc<RefCell<Option<String>>>,
        channel: Channel<SignalingNetworkEvent>,
        recorder: Recorder,
        anchors: Rc<RefCell<Vec<AnchorTag>>>,
    ) -> Self {
        // A previous guard on this thread whose gate was off never took its
        // note — drop it so this run's artifacts can only carry its OWN panic.
        super::panic_note::clear();
        Self { name, description, channel, recorder, anchors, armed: Cell::new(true) }
    }

    /// Stop the guard from firing — the run reports through its normal path.
    pub(super) fn disarm(&self) {
        self.armed.set(false);
    }

    /// Build the report from the live snapshots and write the artifacts under
    /// `<SCENARIO_ARTIFACT_DIR>/<sanitized name>/`. The report's
    /// `scenario_name` is the SAME sanitized segment, so the file stems
    /// `write_all` derives from it are filesystem-safe whatever the harness
    /// name contains. The single failed [`ExpectOutcome`] makes
    /// `RunReport::passed()` false, so the rendered banner reads FAIL; its
    /// detail is the captured panic message when the thread is unwinding, else
    /// a note that `finish()` was never called.
    fn write(&self, sanitized: String, out_dir: &Path, panicking: bool) {
        let detail = if panicking {
            super::panic_note::take()
                .unwrap_or_else(|| "panicked (message not captured)".to_string())
        } else {
            "finish() never reached".to_string()
        };
        let mut report = RunReport::from_recording(
            sanitized,
            self.description.borrow().clone(),
            self.recorder.clone(),
            self.channel.snapshot(),
            Ok(()),
            self.anchors.borrow().clone(),
        );
        report.expects.push(ExpectOutcome {
            agent: self.name.clone(),
            expected: "finish()".to_string(),
            passed: false,
            detail,
        });
        match crate::report::write_all(&report, out_dir) {
            Ok(paths) => eprintln!(
                "[harness] '{}' dropped without finish() — wrote {} artifact(s) under {}",
                self.name,
                paths.len(),
                out_dir.display(),
            ),
            Err(e) => eprintln!(
                "[harness] artifact write for '{}' under {} failed: {e}",
                self.name,
                out_dir.display(),
            ),
        }
    }
}

impl Drop for ArtifactDump {
    fn drop(&mut self) {
        if !self.armed.get() {
            return;
        }
        let dir = match std::env::var(ARTIFACT_DIR_ENV) {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => return,
        };
        let sanitized = seq_report::sanitize_name(&self.name);
        let out_dir = dir.join(&sanitized);
        let panicking = std::thread::panicking();
        // Never panic inside Drop (a second panic while unwinding aborts the
        // process): render + write under catch_unwind, and leave at least a
        // stderr note when the render itself panics (e.g. a mutex poisoned by
        // the very panic being unwound).
        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.write(sanitized, &out_dir, panicking)
        }))
        .is_err()
        {
            eprintln!(
                "[harness] artifact render for '{}' panicked — no artifacts written",
                self.name,
            );
        }
    }
}
