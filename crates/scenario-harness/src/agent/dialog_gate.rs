//! The end-of-run gate on the dialog verdicts of the UAs a harness bound: each
//! verdict a `dialog-…` waiver on its UA does not opt out fails the run, and a
//! waiver that covers one is used.

use std::sync::Arc;

use super::waiver::WaiverState;
use crate::dialogs::{covers, DialogJudge, DialogVerdict};

/// One verdict that survived the waivers, with the UA that recorded it.
pub(crate) struct GatedVerdict {
    pub party: String,
    pub verdict: DialogVerdict,
}

/// The verdicts of `judges` no waiver covers, marking each covering waiver
/// used.
pub(crate) fn surviving_verdicts(
    judges: &[Arc<DialogJudge>],
    waivers: &[WaiverState],
) -> Vec<GatedVerdict> {
    let mut out = Vec::new();
    for judge in judges {
        for verdict in judge.gated() {
            let mut waived = false;
            for w in waivers {
                if covers(&w.scope, judge.party(), &verdict) {
                    w.used.set(true);
                    waived = true;
                }
            }
            if !waived {
                out.push(GatedVerdict { party: judge.party().to_string(), verdict });
            }
        }
    }
    out
}

/// The panic text of a run whose UAs recorded dialog verdicts: one line per
/// verdict, the UA first.
pub(super) fn render_dialog_panic(name: &str, verdicts: &[GatedVerdict]) -> String {
    let lines: Vec<String> = verdicts
        .iter()
        .map(|g| format!("  • {}: {} [{}]", g.party, g.verdict, g.verdict.rule()))
        .collect();
    format!(
        "[{name}] a simulated UA received request(s) its dialog table refuses (RFC 3261 \
         §12.2.2); a test that sends them on purpose opts in with \
         Harness::waive(WaiverScope::rule(\"<rule>\", why).on_party(\"<ua>\")):\n{}",
        lines.join("\n")
    )
}

/// The verdicts as gating audit findings, for a run that collects instead of
/// panicking.
pub(crate) fn as_findings(verdicts: Vec<GatedVerdict>) -> Vec<sip_net::RfcFinding> {
    verdicts
        .into_iter()
        .map(|g| sip_net::RfcFinding {
            rule: g.verdict.rule().to_string(),
            lane: g.party,
            detail: g.verdict.to_string(),
            advisory: false,
            offending: None,
            charged: None,
        })
        .collect()
}
