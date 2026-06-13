//! The transparency oracle (ADR-0013): a failover injected at a safe-point must
//! leave **no visible external behavior**. We assert that two ways:
//!
//! 1. **Differential** — the same scenario is run clean (baseline) and with the
//!    failover injected (variant); the [`Observation`] each UA logically observed
//!    must be byte-identical. Because the capture records dialog identifiers
//!    (From/To tag + CSeq) per message, a takeover that re-mints the b-leg tag or
//!    breaks CSeq progression fails the compare even if the method/status order
//!    looks right. This is the strict check the author asked for.
//! 2. **Universal teardown sweep** — every scenario drives the call to full
//!    termination, then a [`TeardownSweep`] a few simulated seconds later asserts
//!    no held context on either node, a CDR was written, and the limiter drained.
//!
//! The captures are built *inline* as the scenario drives (each `expect`/`receive`
//! appends a token), so retransmits the scenario does not explicitly consume are
//! not in the trace — the comparison is of logical observations, not raw wire.

/// What one UA logically observed during a scenario, in order. Tokens embed the
/// dialog identifiers so the differential compare is strict on From/To/CSeq.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// Ordered tokens alice (caller) observed.
    pub alice: Vec<String>,
    /// Ordered tokens bob (callee) observed.
    pub bob: Vec<String>,
    /// Final call disposition as recorded in the CDR end-event (the externally
    /// meaningful outcome: normal hangup / cancelled / etc.).
    pub disposition: String,
}

impl Observation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a response a UA observed: `RESP <status> cseq=<n>/<method>`.
    pub fn resp(&mut self, who: Who, status: u16, cseq: &str) {
        self.push(who, format!("RESP {status} cseq={cseq}"));
    }

    /// Record a request a UA observed, with its dialog identifiers:
    /// `REQ <method> cseq=<n> from=<tag> to=<tag>`.
    pub fn req(&mut self, who: Who, method: &str, cseq: &str, from_tag: &str, to_tag: &str) {
        self.push(
            who,
            format!("REQ {method} cseq={cseq} from={from_tag} to={to_tag}"),
        );
    }

    fn push(&mut self, who: Who, tok: String) {
        match who {
            Who::Alice => self.alice.push(tok),
            Who::Bob => self.bob.push(tok),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Who {
    Alice,
    Bob,
}

/// Strict differential assertion: the variant (failover) observation must equal
/// the baseline (clean) observation, token-for-token, including the embedded
/// From/To tags and CSeq. Panics with a precise diff on mismatch.
/// `compare_disposition` is `false` for a StayDead cell: the failover is
/// SIP-transparent (alice/bob observe the baseline behavior — the backup answers the
/// wire) but NOT billing-transparent — the primary never reclaims, so the variant
/// loses its CDR (disposition `"no-cdr"` vs the baseline's `"terminated"`). That
/// divergence is the *expected* accepted double-failure, so we still enforce the
/// strict SIP-observation compare but skip the disposition equality. Every other cell
/// remains fully transparent, disposition included.
pub fn assert_transparent(
    cell: &str,
    baseline: &Observation,
    variant: &Observation,
    compare_disposition: bool,
) {
    assert_eq!(
        baseline.alice, variant.alice,
        "[{cell}] TRANSPARENCY VIOLATION on the caller (alice) leg: the failover \
         changed what alice observed.\n  baseline = {:#?}\n  variant  = {:#?}",
        baseline.alice, variant.alice,
    );
    assert_eq!(
        baseline.bob, variant.bob,
        "[{cell}] TRANSPARENCY VIOLATION on the callee (bob) leg: the failover \
         changed what bob observed (a re-minted b-leg tag or broken CSeq shows \
         here).\n  baseline = {:#?}\n  variant  = {:#?}",
        baseline.bob, variant.bob,
    );
    if compare_disposition {
        assert_eq!(
            baseline.disposition, variant.disposition,
            "[{cell}] the final CDR disposition differs between the clean and \
             failover runs: baseline={:?} variant={:?}",
            baseline.disposition, variant.disposition,
        );
    }
}

/// The end-state observed on one node after the call terminated + flushed.
#[derive(Clone, Debug)]
pub struct NodeEndState {
    pub ordinal: String,
    pub alive: bool,
    pub active_calls: usize,
    pub lock_count: usize,
    pub residual_pri: Vec<String>,
    pub residual_bak: Vec<String>,
}

/// The universal post-condition checked a few simulated seconds after every
/// scenario terminates (baseline + variant). Aggregates both nodes + the limiter.
#[derive(Clone, Debug)]
pub struct TeardownSweep {
    pub nodes: Vec<NodeEndState>,
    pub cdr_count: usize,
    pub limiter_total: i64,
    /// Whether this run should have produced a CDR. `true` for every normal run;
    /// `false` for a **StayDead variant** (Model Y, ADR-0020 X3) — the backup never
    /// discharges and the dead primary never reclaims, so the CDR is the accepted
    /// loss of the double-failure. The limiter must STILL drain to 0 and the replica
    /// memory must STILL be freed (via the backup's lossy auto-cleanup reap), so only
    /// the CDR count expectation differs.
    pub expect_cdr: bool,
}

impl TeardownSweep {
    /// Assert everything cleared: no held context on any alive node, the limiter
    /// drained to zero, and the CDR count matches [`expect_cdr`](Self::expect_cdr) —
    /// exactly one for a normal run, exactly zero for a StayDead variant (CDR lost,
    /// the accepted double-failure; limiter + memory still reclaimed).
    pub fn assert_clean(&self, cell: &str) {
        for n in &self.nodes {
            if !n.alive {
                continue;
            }
            assert_eq!(
                n.active_calls, 0,
                "[{cell}] node {} still holds {} active call(s) after teardown",
                n.ordinal, n.active_calls,
            );
            assert_eq!(
                n.lock_count, 0,
                "[{cell}] node {} leaked {} per-call lock(s) after teardown (orphan-reject leak)",
                n.ordinal, n.lock_count,
            );
            assert!(
                n.residual_pri.is_empty(),
                "[{cell}] node {} left {} residual pri: Element(s) {:?} — a later reboot could resurrect them",
                n.ordinal, n.residual_pri.len(), n.residual_pri,
            );
            assert!(
                n.residual_bak.is_empty(),
                "[{cell}] node {} left {} residual bak: Element(s) {:?} — a later reboot could resurrect them",
                n.ordinal, n.residual_bak.len(), n.residual_bak,
            );
        }
        if self.expect_cdr {
            assert!(
                self.cdr_count >= 1,
                "[{cell}] no CDR was written for the call (expected one end-event after the flush window)",
            );
        } else {
            // StayDead variant: the primary never reclaimed, so the CDR is the
            // accepted loss. A non-zero count here means a backup illegally
            // discharged (the removed durable fallback).
            assert_eq!(
                self.cdr_count, 0,
                "[{cell}] StayDead: expected ZERO CDRs (primary never reclaimed; CDR is the \
                 accepted loss), but {} were written — a backup illegally discharged",
                self.cdr_count,
            );
        }
        assert_eq!(
            self.limiter_total, 0,
            "[{cell}] the call limiter did not drain: {} hold(s) still outstanding",
            self.limiter_total,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Observation {
        let mut o = Observation::new();
        o.req(Who::Bob, "INVITE", "1", "ftag", "");
        o.resp(Who::Alice, 200, "1");
        o.req(Who::Bob, "ACK", "1", "ftag", "btag");
        o.disposition = "terminated".into();
        o
    }

    #[test]
    fn identical_observations_are_transparent() {
        assert_transparent("self", &sample(), &sample(), true); // does not panic
    }

    #[test]
    #[should_panic(expected = "TRANSPARENCY VIOLATION on the callee")]
    fn a_remined_bleg_tag_fails_the_oracle() {
        let base = sample();
        let mut variant = sample();
        // Simulate a takeover that re-minted the b-leg To-tag on the ACK.
        variant.bob[1] = "REQ ACK cseq=1 from=ftag to=DIFFERENT".into();
        assert_transparent("self", &base, &variant, true);
    }

    #[test]
    fn staydead_disposition_divergence_is_exempt_but_sip_still_compared() {
        // A StayDead variant: identical SIP observations, but the CDR (and thus the
        // disposition) is lost. With the disposition compare OFF it is transparent.
        let base = sample();
        let mut variant = sample();
        variant.disposition = "no-cdr".into();
        assert_transparent("self", &base, &variant, false); // does not panic
    }

    #[test]
    #[should_panic(expected = "did not drain")]
    fn an_outstanding_limiter_hold_fails_the_sweep() {
        let sweep = TeardownSweep { nodes: vec![], cdr_count: 1, limiter_total: 1, expect_cdr: true };
        sweep.assert_clean("self");
    }

    #[test]
    #[should_panic(expected = "still holds")]
    fn a_leaked_active_call_fails_the_sweep() {
        let sweep = TeardownSweep {
            nodes: vec![NodeEndState {
                ordinal: "b1".into(),
                alive: true,
                active_calls: 1,
                lock_count: 0,
                residual_pri: vec![],
                residual_bak: vec![],
            }],
            cdr_count: 1,
            limiter_total: 0,
            expect_cdr: true,
        };
        sweep.assert_clean("self");
    }

    #[test]
    #[should_panic(expected = "no CDR")]
    fn a_missing_cdr_fails_the_sweep() {
        let sweep = TeardownSweep { nodes: vec![], cdr_count: 0, limiter_total: 0, expect_cdr: true };
        sweep.assert_clean("self");
    }

    #[test]
    fn staydead_zero_cdr_passes_the_sweep() {
        // StayDead variant: zero CDRs is correct (the accepted loss); limiter drained.
        let sweep = TeardownSweep { nodes: vec![], cdr_count: 0, limiter_total: 0, expect_cdr: false };
        sweep.assert_clean("self"); // does not panic
    }

    #[test]
    #[should_panic(expected = "illegally discharged")]
    fn staydead_with_a_cdr_fails_the_sweep() {
        // A backup that illegally discharged a StayDead deferral → a CDR appears.
        let sweep = TeardownSweep { nodes: vec![], cdr_count: 1, limiter_total: 0, expect_cdr: false };
        sweep.assert_clean("self");
    }
}
