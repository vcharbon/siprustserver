//! The one HTML rendering of a load run. Everything it shows comes from a
//! [`LoadRunIndex`], so the page a load generator serves live, the
//! `index.html` it writes at the end of its run and the page a website embeds
//! show the same figures.
//!
//! The page, in order: the command line and target, the headline figures
//! (with the rate and drain controls on a live page), the time series, the
//! last failures, the results by scenario × class × case, the latency,
//! checkpoint, round-trip and check tables, the rate events and the run's
//! settings. Plain HTML and inline SVG: no script.

use e2e_model::LoadRunIndex;

mod headline;
mod html;
mod results;
mod series;
mod status;
mod tables;

pub use html::fmt_epoch_ms;
pub use status::status_line;

/// The page's minimal style sheet, for a host page that embeds the body.
pub const CSS: &str = "body{font-family:sans-serif}table{border-collapse:collapse;margin:.5em 0}\
td,th{border:1px solid #ccc;padding:4px 8px;text-align:left}.ok{color:#070}.nok{color:#a00}\
.muted{color:#666}pre.cmd{background:#f4f4f4;padding:6px;white-space:pre-wrap}\
.spark{display:inline-block;margin:0 1.5em .5em 0}.spark svg{vertical-align:middle;border:1px solid #ddd}\
form.inline{display:inline;margin-right:1em}";

/// How a page is rendered.
#[derive(Debug, Clone, Default)]
pub struct RenderOpts {
    /// Prepended to each run-dir-relative sample page path to form its link
    /// (empty when the page sits at the run-dir root).
    pub sample_prefix: String,
    /// The page is served by the running load generator: it carries the rate
    /// and drain controls and refreshes itself. Ignored once the index is
    /// finished.
    pub live: Option<Live>,
}

/// The live page's refresh period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Live {
    /// Seconds between automatic reloads; `0` freezes the page.
    pub refresh_secs: u32,
}

/// The default automatic reload period of a live page, in seconds.
pub const DEFAULT_REFRESH_SECS: u32 = 60;

impl RenderOpts {
    /// The live settings, when the page is live and the run not finished.
    fn live_for(&self, idx: &LoadRunIndex) -> Option<Live> {
        self.live.filter(|_| !idx.meta.finished)
    }
}

/// The report as an HTML fragment, for a host page that brings its own
/// document and style ([`CSS`]).
pub fn render_body(idx: &LoadRunIndex, opts: &RenderOpts) -> String {
    let live = opts.live_for(idx);
    let mut out = String::new();
    headline::command(&mut out, idx);
    headline::headline(&mut out, idx, live);
    series::series(&mut out, idx);
    results::failures(&mut out, idx, &opts.sample_prefix);
    results::results(&mut out, idx, &opts.sample_prefix);
    tables::latency(&mut out, idx);
    tables::checkpoints(&mut out, idx);
    tables::rtt(&mut out, idx);
    tables::checks(&mut out, idx);
    tables::rate_events(&mut out, idx);
    tables::settings(&mut out, idx);
    out
}

/// The report as a standalone HTML document: [`render_body`] under a title,
/// the style sheet and, on a live page with a non-zero period, an automatic
/// reload.
pub fn render_page(idx: &LoadRunIndex, opts: &RenderOpts) -> String {
    let refresh = match opts.live_for(idx) {
        Some(Live { refresh_secs }) if refresh_secs > 0 => {
            format!("<meta http-equiv=\"refresh\" content=\"{refresh_secs}\">")
        }
        _ => String::new(),
    };
    format!(
        "<!DOCTYPE html><html><head><meta charset=\"utf-8\">{refresh}<title>loadgen report</title>\
         <style>{CSS}</style></head><body><h1>loadgen report</h1>{}</body></html>",
        render_body(idx, opts)
    )
}

#[cfg(test)]
mod tests;
