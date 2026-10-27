//! The RFC audit gates of a [`FailoverHarness`] run: the endpoint-scoped
//! cross-message audit every cell runs, and the full per-bind suite a via-LB
//! test opts into. A deviation an incident causes is accepted only by the
//! window a strike opened ([`FailoverHarness::strike`]).

use std::collections::HashSet;

use layer_harness::Stamped;
use sip_net_testkit::{audit_visible_event, CaptureIndex, SignalingNetworkEvent};

use super::FailoverHarness;
use crate::rfc_acceptance::{lane_details, Accepted, Finding};

impl FailoverHarness {
    /// Run the FULL RFC audit suite — the per-bind **peer** rules (Via echo /
    /// response↔transaction correlation, tags, CANCEL/RAck correlation, …) on
    /// top of the cross-message rules — over the recorded trace and panic on
    /// any non-advisory finding, honouring
    /// [`allow_rfc_violation`](Self::allow_rfc_violation) waivers.
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
        // `offending` is a 1-based index into the AUDIT-visible wire entries — the
        // same view `evaluate_rfc_findings` hands the rules — so a window must be
        // resolved against `audit_wire_entries`, never the raw snapshot (whose
        // extra ReEmit / invisible-disposition rows shift every later index).
        let capture = CaptureIndex::new(sip_net_testkit::audit_wire_entries(&events));
        let findings: Vec<_> = sip_net_testkit::evaluate_rfc_findings(&events)
            .into_iter()
            .filter(|f| !f.advisory && !self.rfc_acceptance.waived(&f.rule))
            .filter(|f| self.rfc_acceptance.accepts(&f.rule, f.offending, &capture).is_none())
            .collect();
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
        lane_details(self.partition_rfc_findings().0)
    }

    /// Split the endpoint-scoped cross-message audit into `(gating, accepted)` —
    /// see [`RfcAcceptance::partition`](crate::rfc_acceptance::RfcAcceptance::partition).
    pub(super) fn partition_rfc_findings(&self) -> (Vec<Finding>, Vec<Accepted>) {
        let (events, scoped_out) = self.audited_events();
        self.rfc_acceptance.partition(&events, &scoped_out)
    }

    /// The findings a window of a struck row covered this run, each with the
    /// row that took it. They do not gate, but they are recorded (advisory
    /// anomalies in the unified report) so an accepted deviation stays
    /// visible, and a test can assert on them to prove its window matched
    /// something.
    pub fn accepted_rfc_deviations(&self) -> Vec<Accepted> {
        self.partition_rfc_findings().1
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
            .chain(self.relay_sip_addrs.iter())
            .chain(incarnation_binds.iter())
            .map(|a| a.to_string())
            .collect();
        let snapshot = self.harness.recording().channel().snapshot();
        let events = snapshot
            .into_iter()
            .filter(|s| !worker_binds.contains(s.event.bind_key()) && audit_visible_event(&s.event))
            .collect();
        (events, worker_binds)
    }
}
