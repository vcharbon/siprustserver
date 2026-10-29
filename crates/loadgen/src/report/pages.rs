//! The stored sample pages, by run-dir-relative path: what the report writes
//! to disk and the live surface serves from memory.

use std::sync::Arc;

use super::{sample_rel, Reporter};
use crate::report::RenderedSample;

impl Reporter {
    /// Every stored sample page, by run-dir-relative path.
    pub fn sample_pages(&self) -> Vec<(String, Arc<str>)> {
        let g = self.inner.lock().unwrap();
        let mut pages = Vec::new();
        for ((scenario, class, case), samples) in &g.samples {
            for (i, s) in samples.iter().enumerate() {
                pages.push((
                    sample_rel(scenario, class, case, i),
                    page_of(s, &format!("{scenario} / {class} / {case} #{i}")),
                ));
            }
        }
        pages
    }

    /// The stored sample page at run-dir-relative path `rel`, if any.
    pub fn sample_page(&self, rel: &str) -> Option<Arc<str>> {
        let g = self.inner.lock().unwrap();
        g.samples.iter().find_map(|((scenario, class, case), samples)| {
            samples.iter().enumerate().find_map(|(i, s)| {
                (sample_rel(scenario, class, case, i) == rel)
                    .then(|| page_of(s, &format!("{scenario} / {class} / {case} #{i}")))
            })
        })
    }
}

/// A sample's page: its callflow, or for a call that never ran (no flow) a
/// stub that still states its detail.
fn page_of(sample: &RenderedSample, heading: &str) -> Arc<str> {
    match &sample.html {
        Some(html) => html.clone(),
        None => Arc::from(format!(
            "<html><body><h3>{}</h3><p>{}</p><p>e2e: {:.1} ms</p></body></html>",
            escape(heading),
            escape(sample.detail.as_deref().unwrap_or("(no detail)")),
            sample.e2e_ms
        )),
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}
