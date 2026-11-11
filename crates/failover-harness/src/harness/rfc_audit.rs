//! The RFC audit gates of a [`FailoverHarness`] run: the endpoint-scoped
//! cross-message audit every cell runs, and the full per-bind suite a via-LB
//! test opts into. A deviation an incident causes is accepted only by the
//! window a strike opened ([`FailoverHarness::strike`]).

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;

use layer_harness::{NetworkTag, Stamped};
use sip_net_testkit::{cross_audit_event, SignalingNetworkEvent};

use super::FailoverHarness;
use crate::rfc_acceptance::{lane_details, Accepted, Finding, Partitioned};

impl FailoverHarness {
    /// Run the FULL RFC audit suite — the per-bind **peer** rules (Via echo /
    /// response↔transaction correlation, tags, CANCEL/RAck correlation, …) on
    /// top of the cross-message rules — over the recorded trace and panic on
    /// any non-advisory finding, honouring
    /// [`allow_rfc_violation`](Self::allow_rfc_violation) waivers. A finding
    /// informative inside the system under test, on a hop between two of its
    /// parties ([`sut_addrs`](Self::sut_addrs)), does not gate.
    ///
    /// The Drop-time gate deliberately runs only the endpoint-scoped
    /// cross-message rules (a transparent failover splits one dialog's CSeq
    /// stream across worker binds, so per-bind peer rules would report phantom
    /// findings on the internal nodes). A NON-failover via-LB test — where no
    /// call ever changes workers — can and should opt into the full per-bind
    /// suite, which is exactly what gates the downstream e2e runs:
    /// it is the only lane that judges the **relay (proxy) bind's** own client
    /// transactions (e.g. `response-echoes-request-via` §8.1.3/§17.1.3 response matching).
    pub fn assert_full_rfc_clean(&self, cell: &str) {
        let events = self.harness.recording().channel().snapshot();
        let findings = self.rfc_acceptance.full_suite_gating(&events, self.is_sut());
        assert!(
            findings.is_empty(),
            "[{cell}] full-suite RFC audit violation(s) on the recorded trace:\n{}",
            findings
                .iter()
                .map(|f| format!("  • [{}] {}: {}", f.lane, f.rule, f.detail))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    /// Run the built-in RFC 3261 signaling audit (CSeq in-dialog ordering, …)
    /// over the recorded SIP trace and panic on any violation — the SIP-plane
    /// analogue of the universal teardown sweep's "all clean" check, applied to
    /// EVERY cell once the scenario has fully run. Reads the recording channel
    /// directly (no layer-close structural checks, no consume), so it can run
    /// mid-life on the long-lived multi-SUT harness. Catches a takeover that
    /// probes a dialog with a stale CSeq — a regression a real UAC rejects as
    /// `unexpected_msg` but a test UA answers silently.
    pub fn assert_sip_rfc_clean(&self, cell: &str) {
        let findings = self.rfc_audit_findings();
        assert!(
            findings.is_empty(),
            "[{cell}] SIP RFC audit violation(s) on the recorded trace \
             (a real UA would have rejected these):\n{}",
            findings
                .iter()
                .map(|(lane, detail)| format!("  • [{lane}] {detail}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }

    /// The RFC 3261 cross-message audit findings that GATE — everything the
    /// recorded trace violated except what an acceptance window covers (the
    /// `(lane, detail)` pairs `assert_sip_rfc_clean` panics on). Reads the
    /// recording channel snapshot NON-consuming, so it is safe to call mid-run AND
    /// from `Drop`. Empty ⇒ clean. Shared by the explicit `assert_sip_rfc_clean`
    /// and the automatic Drop-time enforcement so the SAME rule set runs on every
    /// FailoverHarness-based test with no per-test opt-in.
    pub(super) fn rfc_audit_findings(&self) -> Vec<(String, String)> {
        lane_details(self.partition_rfc_findings().gating)
    }

    /// Classify the endpoint-scoped cross-message audit — see
    /// [`RfcAcceptance::partition`](crate::rfc_acceptance::RfcAcceptance::partition).
    pub(super) fn partition_rfc_findings(&self) -> Partitioned {
        let (events, scoped_out) = self.audited_events();
        self.rfc_acceptance.partition(&events, &scoped_out, self.is_sut())
    }

    /// Every address the run bound as a system under test: each lane bound as
    /// SUT (the proxy's binds, every worker incarnation) and every worker SIP
    /// address ever bound. The simulated callers and callees are not.
    pub fn sut_addrs(&self) -> HashSet<SocketAddr> {
        let bound = self.harness.recording().recorder().snapshot().lanes;
        bound
            .into_iter()
            .filter(|l| l.network == NetworkTag::Core)
            .map(|l| l.addr)
            .chain(self.all_worker_sip_addrs.iter().copied())
            .chain(self.views.all_sip_addrs())
            .collect()
    }

    /// Whether a bind key names a party under test ([`sut_addrs`](Self::sut_addrs)).
    fn is_sut(&self) -> impl Fn(&str) -> bool {
        let sut = self.sut_addrs();
        move |bind| sip_net_testkit::report::lane_addr(bind).is_some_and(|at| sut.contains(&at))
    }

    /// The findings a window of a struck row covered this run, each with the
    /// row that took it. They do not gate, but they are recorded (advisory
    /// anomalies in the unified report) so an accepted deviation stays
    /// visible, and a test can assert on them to prove its window matched
    /// something.
    pub fn accepted_rfc_deviations(&self) -> Vec<Accepted> {
        self.partition_rfc_findings().accepted
    }

    /// The findings this run recorded as informative: a rule informative
    /// inside the system under test, on a hop between two of its parties.
    /// They never gate; the unified report lists them as advisory anomalies.
    pub fn informative_rfc_findings(&self) -> Vec<Finding> {
        self.partition_rfc_findings().informative
    }

    /// Fold the run's RFC status into `doc`: a trace that violates a gating
    /// rule never shows PASS, so each gating finding is a gating anomaly and
    /// `passed` turns false. A window-accepted finding, a finding informative
    /// inside the system under test, and each rule waived for the whole run
    /// (with its justification) are listed as advisory: classified, never
    /// masked, and they do not fail the run.
    pub(super) fn fold_rfc_findings(&self, doc: &mut seq_report::SeqDoc) {
        let (gates, anomalies) =
            rfc_anomalies(self.partition_rfc_findings(), self.rfc_acceptance.lifetime_waivers());
        if gates {
            doc.passed = false;
        }
        doc.anomalies.extend(anomalies);
    }

    /// The recorded SIP events the audit judges — the endpoint-scoped view —
    /// and the worker binds taken out of it.
    fn audited_events(&self) -> (Vec<Stamped<SignalingNetworkEvent>>, HashSet<String>) {
        // RFC 3261 conformance is only observable from OUTSIDE the proxy/LB — at
        // the real UAs (alice/bob). A *transparent* failover legitimately splits
        // ONE logical dialog across several cluster workers: alice's in-dialog
        // CSeq stream 1→2→3 can land 1 on the primary, 2 on the backup (takeover),
        // 3 on the reclaimed primary. Auditing per **worker** bind then reports a
        // phantom "CSeq 3 skips ahead of CSeq 1" on the worker that happened to
        // serve 1 and 3 — even though alice SENT 1,2,3 contiguously and bob
        // RECEIVED 1,2,3 contiguously on its leg. No real UA ever sees that gap.
        //
        // So scope the audit to the dialog as observed at the endpoints, not at
        // the internal cluster nodes: drop the worker binds. Equivalent to
        // regrouping per dialog independent of which worker handled each request —
        // the surviving streams (alice, bob, and the proxy, which forwards EVERY
        // request and so sees the whole 1,2,3 sequence) are each CSeq-monotonic.
        // The endpoint/proxy binds still catch a genuine same-leg CSeq collision
        // (e.g. a keepalive OPTIONS and a later BYE reusing one CSeq toward bob).
        let incarnation_binds = self.views.all_sip_addrs();
        let worker_binds: HashSet<String> = self
            .all_worker_sip_addrs
            .iter()
            .chain(incarnation_binds.iter())
            .map(|a| a.to_string())
            .chain(self.relay_lanes.iter().cloned())
            .collect();
        let snapshot = self.harness.recording().channel().snapshot();
        let events = snapshot
            .into_iter()
            .filter(|s| !worker_binds.contains(s.event.bind_key()) && cross_audit_event(&s.event))
            .collect();
        (events, worker_binds)
    }
}

/// The report's RFC anomalies of one classified run, and whether any gates:
/// each gating finding a gating anomaly; each window-accepted finding, each
/// finding informative inside the system under test and each rule waived for
/// the run (with its justification) an advisory one.
fn rfc_anomalies(
    partitioned: Partitioned,
    waivers: &BTreeMap<String, String>,
) -> (bool, Vec<seq_report::Anomaly>) {
    let Partitioned { gating, accepted, informative } = partitioned;
    let anomaly =
        |check: String, detail: String, lane: Option<String>, advisory: bool| seq_report::Anomaly {
            check,
            detail,
            lane,
            endpoint: None,
            advisory: Some(advisory),
            row_seqs: Vec::new(),
            rule_sourced: true,
        };
    let gates = !gating.is_empty();
    let mut out = Vec::new();
    for (rule, lane, detail) in gating {
        out.push(anomaly(rule, detail, Some(lane), false));
    }
    for ((rule, lane, detail), row) in accepted {
        out.push(anomaly(rule, format!("ACCEPTED by {row}: {detail}"), Some(lane), true));
    }
    for (rule, lane, detail) in informative {
        let detail = format!("INFORMATIVE inside the system under test: {detail}");
        out.push(anomaly(rule, detail, Some(lane), true));
    }
    for (rule, why) in waivers {
        out.push(anomaly(rule.clone(), format!("WAIVED for the run: {why}"), None, true));
    }
    (gates, out)
}

#[cfg(test)]
mod tests;
