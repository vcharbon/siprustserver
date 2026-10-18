//! The end-of-run gate on the dialog verdicts of the UAs a harness bound: each
//! verdict a `dialog-…` waiver on its UA does not opt out fails the run, and a
//! waiver that covers one is used.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::waiver::WaiverState;
use crate::dialogs::{covers, DialogJudge, DialogVerdict};

/// One verdict that survived the waivers, with the UA that recorded it.
pub(crate) struct GatedVerdict {
    pub party: String,
    pub verdict: DialogVerdict,
}

/// The verdicts of `judges` no waiver covers, marking each covering waiver
/// used, and per rule the count of verdicts a waiver covered: every dialog
/// verdict is a finding against the SUT that sent the message.
pub(crate) fn surviving_verdicts(
    judges: &[Arc<DialogJudge>],
    waivers: &[WaiverState],
) -> (Vec<GatedVerdict>, BTreeMap<String, usize>) {
    let mut out = Vec::new();
    let mut waived_by_rule = BTreeMap::new();
    for judge in judges {
        for verdict in judge.gated() {
            let mut waived = false;
            for w in waivers {
                if covers(&w.scope, judge.party(), &verdict) {
                    w.used.set(true);
                    waived = true;
                }
            }
            if waived {
                *waived_by_rule.entry(verdict.rule().to_string()).or_insert(0) += 1;
            } else {
                out.push(GatedVerdict { party: judge.party().to_string(), verdict });
            }
        }
    }
    (out, waived_by_rule)
}

/// The panic text of a run whose UAs recorded dialog verdicts: a first line
/// naming the first rule that fired and its UA, then one line per verdict.
pub(crate) fn render_dialog_panic(name: &str, verdicts: &[GatedVerdict]) -> String {
    let more = match verdicts.len() {
        0 | 1 => String::new(),
        n => format!(" (and {} more verdict(s))", n - 1),
    };
    let first = verdicts
        .first()
        .map(|g| format!("dialog rule {} fired on {}: {}", g.verdict.rule(), g.party, g.verdict))
        .unwrap_or_default();
    let lines: Vec<String> = verdicts
        .iter()
        .map(|g| format!("  • {}: {} [{}]", g.party, g.verdict, g.verdict.rule()))
        .collect();
    format!(
        "red because {first}{more}\n[{name}] a simulated UA received request(s) its dialog \
         table refuses (RFC 3261 §12.2.2); a test that sends them on purpose opts in with \
         Harness::waive(WaiverScope::rule(\"<rule>\", why).on_party(\"<ua>\")):\n{}",
        lines.join("\n")
    )
}

/// The verdicts as gating audit findings, for a run that collects instead of
/// panicking.
pub(crate) fn as_findings(verdicts: Vec<GatedVerdict>) -> Vec<sip_net_testkit::RfcFinding> {
    verdicts
        .into_iter()
        .map(|g| sip_net_testkit::RfcFinding {
            rule: g.verdict.rule().to_string(),
            lane: g.party,
            detail: g.verdict.to_string(),
            advisory: false,
            offending: None,
            charged: None,
        })
        .collect()
}
