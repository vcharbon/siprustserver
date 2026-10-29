//! The on-disk report as text: the sample pages, `index.html` and
//! `summary.md`, rendered from one snapshot of the reporter.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{sample_rel, Bucket, Inner, Reporter, ReporterCfg, CHASE_CALLS};
use crate::class::ResultClass;
use crate::scenarios::ScenarioId;

/// The on-disk report rendered under the reporter's lock, written after it.
pub(super) struct ReportFiles {
    /// Every sample page, by run-dir-relative path.
    pub(super) pages: Vec<(String, Arc<str>)>,
    pub(super) index: String,
    pub(super) summary: String,
}

impl Reporter {
    /// The on-disk report as text: every sample page (by run-dir-relative
    /// path), `index.html` and `summary.md`.
    pub(super) fn render_report(&self) -> ReportFiles {
        use std::fmt::Write as _;
        let g = self.inner.lock().unwrap();

        // Each sample's callflow HTML, split by case (which failure mode).
        let mut pages: Vec<(String, Arc<str>)> = Vec::new();
        let mut links: BTreeMap<Bucket, Vec<String>> = BTreeMap::new();
        for ((scenario, class, case), samples) in &g.samples {
            for (i, s) in samples.iter().enumerate() {
                let rel = sample_rel(scenario, class, case, i);
                let html = match &s.html {
                    Some(html) => html.clone(),
                    // A call that never ran has no flow: a stub so its detail
                    // is still linked.
                    None => Arc::from(format!(
                        "<html><body><h3>{scenario} / {class} / {case} #{i}</h3><p>{}</p>\
                         <p>e2e: {:.1} ms</p></body></html>",
                        s.detail.as_deref().unwrap_or("(no detail)"),
                        s.e2e_ms
                    )),
                };
                pages.push((rel.clone(), html));
                links.entry((scenario, class.clone(), case.clone())).or_default().push(rel);
            }
        }

        // index.html
        let mut idx = String::new();
        idx.push_str(
            "<html><head><meta charset=\"utf-8\"><title>loadgen report</title>\
            <style>body{font-family:sans-serif}table{border-collapse:collapse}\
            td,th{border:1px solid #ccc;padding:4px 8px}.ok{color:#070}.nok{color:#a00}</style>\
            </head><body><h1>loadgen report</h1>",
        );
        idx.push_str(
            "<h2>Results by scenario × class × case</h2>\
            <p>The <b>case</b> column splits a class into its distinct failure modes (RFC rule id, \
            failed check, failed step) — each keeps its own first-N samples. The RFC audit's \
            trace rules and the case checks judge a call's recording: <b>judged share</b> is a \
            failure's share of the recorded calls they judged, and its estimate over the calls \
            that ended on an accepted branch.</p>",
        );
        for line in self.recording_lines(&g) {
            idx.push_str(&format!("<p>{line}</p>"));
        }
        idx.push_str(
            "<table><tr><th>scenario</th><th>class</th><th>case</th><th>count</th>\
            <th>judged share</th><th>samples</th></tr>",
        );
        let none_recorded = match (self.cfg.sample_cap, self.cfg.background_record_every) {
            (0, _) => String::new(),
            (_, 0) => "none recorded".to_string(),
            (_, every) => format!("none recorded (1 in {every} calls is recorded)"),
        };
        for ((scenario, class, case), n) in &g.counts {
            let cls = if ResultClass::label_is_ok(class) { "ok" } else { "nok" };
            let sample_links = links
                .get(&(scenario, class.clone(), case.clone()))
                .map(|ls| {
                    ls.iter()
                        .enumerate()
                        .map(|(i, l)| format!("<a href=\"{l}\">{i}</a>"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_else(|| none_recorded.clone());
            let share = judged_share(&g, &(scenario, class.clone(), case.clone()), *n);
            idx.push_str(&format!(
                "<tr><td>{scenario}</td><td class=\"{cls}\">{class}</td><td>{case}</td>\
                 <td>{n}</td><td>{share}</td><td>{sample_links}</td></tr>"
            ));
        }
        idx.push_str("</table><h2>Latency (ms)</h2><table>\
            <tr><th>scenario</th><th>n</th><th>mean</th><th>p50</th><th>p90</th><th>p99</th><th>max</th></tr>");
        for (scenario, h) in &g.e2e {
            idx.push_str(&format!(
                "<tr><td>{scenario}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td></tr>",
                h.total, h.mean_ms(), h.quantile_ms(0.5), h.quantile_ms(0.9), h.quantile_ms(0.99), h.max
            ));
        }
        idx.push_str("</table>");
        if !g.checkpoints.is_empty() {
            idx.push_str("<h2>Checkpoints (ms)</h2><table>\
                <tr><th>scenario</th><th>checkpoint</th><th>n</th><th>p50</th><th>p90</th><th>p99</th></tr>");
            for ((scenario, name), h) in &g.checkpoints {
                idx.push_str(&format!(
                    "<tr><td>{scenario}</td><td>{name}</td><td>{}</td><td>{:.1}</td><td>{:.1}</td><td>{:.1}</td></tr>",
                    h.total, h.quantile_ms(0.5), h.quantile_ms(0.9), h.quantile_ms(0.99)
                ));
            }
            idx.push_str("</table>");
        }
        idx.push_str("</body></html>");

        // summary.md (writing into a String cannot fail).
        let mut md = String::new();
        let _ = writeln!(md, "# loadgen summary\n");
        let _ = writeln!(md, "## Results by scenario × class × case\n");
        let _ = writeln!(md, "(case = the class's distinct failure mode)\n");
        for line in self.recording_lines(&g) {
            let _ = writeln!(md, "{}\n", line.replace("<b>", "**").replace("</b>", "**"));
        }
        let _ = writeln!(md, "| scenario | class | case | count | judged share |");
        let _ = writeln!(md, "|---|---|---|---|---|");
        for ((scenario, class, case), n) in &g.counts {
            let share = judged_share(&g, &(scenario, class.clone(), case.clone()), *n);
            let _ = writeln!(md, "| {scenario} | {class} | {case} | {n} | {share} |");
        }
        let _ = writeln!(md, "\n## Latency (ms)\n");
        let _ = writeln!(md, "| scenario | n | mean | p50 | p90 | p99 | max |");
        let _ = writeln!(md, "|---|---|---|---|---|---|---|");
        for (scenario, h) in &g.e2e {
            let _ = writeln!(
                md,
                "| {scenario} | {} | {:.1} | {:.1} | {:.1} | {:.1} | {:.1} |",
                h.total,
                h.mean_ms(),
                h.quantile_ms(0.5),
                h.quantile_ms(0.9),
                h.quantile_ms(0.99),
                h.max
            );
        }
        ReportFiles { pages, index: idx, summary: md }
    }

    /// One line per scenario: the calls placed and recorded, the calls the RFC
    /// audit judged and those whose recording it judged, and the calls the
    /// case checks judged.
    fn recording_lines(&self, g: &Inner) -> Vec<String> {
        let ReporterCfg { sample_cap, background_record_every: every } = self.cfg;
        let mut gates = Vec::new();
        if sample_cap > 0 {
            gates.push(format!("the first {sample_cap}"));
        }
        if every > 0 {
            gates.push(format!("1 in {every}"));
        }
        gates.push(format!("{CHASE_CALLS} after each new failure mode"));
        let gates = gates.join(", ");
        g.placed
            .iter()
            .map(|(scenario, placed)| {
                let tally = |m: &BTreeMap<ScenarioId, u64>| m.get(scenario).copied().unwrap_or(0);
                let mut line = format!(
                    "<b>{scenario}</b>: {placed} calls placed, {} recorded ({gates}). {} ended on \
                     an accepted branch; the RFC audit judged the recording of {} of them",
                    tally(&g.recorded),
                    tally(&g.accepted),
                    tally(&g.audited),
                );
                if let Some((passed, failed)) = g.check_verdicts.get(scenario) {
                    line.push_str(&format!("; the case checks judged {}", passed + failed));
                }
                line.push('.');
                line
            })
            .collect()
    }
}

/// A row's share of the recorded calls its class judges on their recording,
/// and the estimate over every call that class judges: an `rfc_audit_fail`
/// row counts its audited recorded calls over the audited ones, estimated
/// over the calls that ended on an accepted branch; a `check_fail` row counts
/// its calls over the checked ones, estimated over the accepted calls clean of
/// the audit. Every other row is judged on every call and shows no share, and
/// so does an `rfc_audit_fail` row with no audited recorded call.
fn judged_share(g: &Inner, bucket: &Bucket, n: u64) -> String {
    let (scenario, class, _) = bucket;
    let tally = |m: &BTreeMap<ScenarioId, u64>| m.get(scenario).copied().unwrap_or(0);
    let (k, judged, base, noun) = if *class == ResultClass::RfcAuditFail.label() {
        let k = g.audited_failed.get(bucket).copied().unwrap_or(0);
        (k, tally(&g.audited), tally(&g.accepted), "audited")
    } else if *class == ResultClass::CheckFail.label() {
        let (passed, failed) = g.check_verdicts.get(scenario).copied().unwrap_or((0, 0));
        let count = |label: String| -> u64 {
            g.counts
                .iter()
                .filter(|((s, c, _), _)| s == scenario && *c == label)
                .map(|(_, n)| n)
                .sum()
        };
        let base = count(ResultClass::Ok.label()) + count(ResultClass::CheckFail.label());
        (n, passed + failed, base, "checked")
    } else {
        return String::new();
    };
    if k == 0 || judged == 0 {
        return String::new();
    }
    let share = k as f64 / judged as f64;
    let estimate = (share * base as f64).round();
    format!("{k} of {judged} {noun} ({:.1} %), ≈ {estimate} of {base}", share * 100.0)
}
