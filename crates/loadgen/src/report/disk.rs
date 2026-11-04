//! The report on disk: the sample pages, `index.html` and `load-result.json`
//! of one index, written once or periodically.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use e2e_model::{Canaries, LoadRunIndex, LoadRunMeta};

use super::Reporter;

impl Reporter {
    /// Write the report of `index` under `out_dir`: every stored sample page,
    /// then `index.html` (the page rendered from `index`) and
    /// `load-result.json`. The pages are copied out under the reporter's lock
    /// and written after it is released, so recording calls never wait on the
    /// disk; samples only accrue, so every page `index` lists is written.
    pub fn write_report(&self, out_dir: &Path, index: &LoadRunIndex) -> std::io::Result<()> {
        std::fs::create_dir_all(out_dir)?;
        for (rel, html) in self.sample_pages() {
            let page = out_dir.join(&rel);
            if let Some(parent) = page.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&page, html.as_bytes())?;
        }
        let html = loadrun_render::render_page(index, &loadrun_render::RenderOpts::default());
        std::fs::write(out_dir.join("index.html"), html)?;
        let json = serde_json::to_string_pretty(index)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(out_dir.join("load-result.json"), json + "\n")
    }

    /// Write the report of the reporter alone (no run settings, no mux
    /// canaries), finished, under `out_dir`.
    pub fn finalize(&self, out_dir: &Path) -> std::io::Result<()> {
        let meta = LoadRunMeta { finished: true, ..LoadRunMeta::default() };
        self.write_report(out_dir, &self.build_index(meta, Canaries::default()))
    }

    /// Spawn the periodic on-disk snapshot task: every `every` it writes the
    /// report of `index()` (via [`write_report`](Self::write_report)) so the
    /// report is browsable on disk mid-run. `index` must stamp `finished:
    /// false`: a snapshot is never the final word.
    ///
    /// SHUTDOWN ORDERING: the caller MUST `abort()` the returned handle and
    /// await it BEFORE the final write. The task loops forever; left running,
    /// a tick that lands on the run's last instant races the final write and
    /// strands `finished: false` on disk.
    pub fn spawn_snapshots(
        self: &Arc<Self>,
        out_dir: PathBuf,
        every: Duration,
        index: impl Fn() -> LoadRunIndex + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let reporter = Arc::clone(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                if let Err(e) = reporter.write_report(&out_dir, &index()) {
                    eprintln!("[loadgen] periodic report snapshot failed: {e}");
                }
            }
        })
    }
}
