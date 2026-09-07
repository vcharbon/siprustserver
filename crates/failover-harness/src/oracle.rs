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
//! appends a token), so a datagram the scenario never pulls is not in the trace —
//! the comparison is of logical observations, not raw wire.
//!
//! **The retransmission fold (ADR-0029 X3, X5).** A rung of a retransmission
//! ladder is THE datagram it repeats, byte for byte — so a datagram byte-identical
//! to the one a UA observed last from the same emitter is a rung, and folds into
//! the token that datagram already produced. How many rungs a window held open
//! collects (the failover injection advances the clock, the clean baseline does
//! not) therefore never shows in the compare, while a "repeat" that differs by a
//! byte — a takeover node re-composing a 2xx instead of re-sending it — is a
//! token of its own and fails the cell. The fold is the oracle asserting X3, not
//! tolerating retransmission.

use sip_message::{SipRequest, SipResponse};

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
    /// Per UA, the datagram its last token was read from — what a rung is
    /// byte-identical to.
    last_datagram: [Option<Vec<u8>>; 2],
}

impl Observation {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a response a UA observed, as `RESP <status> cseq=<n>` — or fold
    /// it into the previous token when it is that datagram again (a rung).
    /// Returns whether a token was appended.
    pub fn resp(&mut self, who: Who, r: &SipResponse) -> bool {
        let tok = format!("RESP {} cseq={}", r.status(), r.cseq().seq());
        self.observe(who, r.image(), tok)
    }

    /// Record a request a UA observed with its dialog identifiers, as
    /// `REQ <method> cseq=<n> from=<tag> to=<tag>` — or fold it into the
    /// previous token when it is that datagram again (a rung). Returns whether
    /// a token was appended.
    pub fn req(&mut self, who: Who, r: &SipRequest) -> bool {
        let tok = format!(
            "REQ {} cseq={} from={} to={}",
            r.method().as_str(),
            r.cseq().seq(),
            r.from().tag().unwrap_or_default(),
            r.to().tag().unwrap_or_default(),
        );
        self.observe(who, r.image(), tok)
    }

    /// The fold: a datagram byte-identical to the one `who` observed last is a
    /// rung of that token's ladder and appends nothing; any other datagram —
    /// including one that differs from its predecessor by a single byte — is
    /// the next token.
    fn observe(&mut self, who: Who, datagram: &[u8], tok: String) -> bool {
        let last = &mut self.last_datagram[who as usize];
        if last.as_deref() == Some(datagram) {
            return false;
        }
        *last = Some(datagram.to_vec());
        match who {
            Who::Alice => self.alice.push(tok),
            Who::Bob => self.bob.push(tok),
        }
        true
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
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser};

    use super::*;

    const INVITE: &str = "INVITE sip:bob@127.0.0.1:5070 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK1\r\nFrom: <sip:alice@127.0.0.1>;tag=ftag\r\nTo: <sip:bob@127.0.0.1>\r\nCall-ID: c1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
    const OK_200: &str = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK1\r\nFrom: <sip:alice@127.0.0.1>;tag=ftag\r\nTo: <sip:bob@127.0.0.1>;tag=btag\r\nCall-ID: c1\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n";
    const ACK: &str = "ACK sip:bob@127.0.0.1:5070 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bK2\r\nFrom: <sip:alice@127.0.0.1>;tag=ftag\r\nTo: <sip:bob@127.0.0.1>;tag=btag\r\nCall-ID: c1\r\nCSeq: 1 ACK\r\nContent-Length: 0\r\n\r\n";

    fn request(raw: &str) -> SipRequest {
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("fixture is a request"),
        }
    }

    fn response(raw: &str) -> SipResponse {
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Response(r) => r,
            SipMessage::Request(_) => panic!("fixture is a response"),
        }
    }

    fn sample() -> Observation {
        let mut o = Observation::new();
        o.req(Who::Bob, &request(INVITE));
        o.resp(Who::Alice, &response(OK_200));
        o.req(Who::Bob, &request(ACK));
        o.disposition = "terminated".into();
        o
    }

    #[test]
    fn tokens_carry_the_dialog_identifiers() {
        let o = sample();
        assert_eq!(o.alice, vec!["RESP 200 cseq=1"]);
        assert_eq!(
            o.bob,
            vec!["REQ INVITE cseq=1 from=ftag to=", "REQ ACK cseq=1 from=ftag to=btag"]
        );
    }

    /// The rungs of a ladder — the same datagram again, however many times —
    /// fold into the token the original produced, so a window that held open
    /// for more rungs compares equal to one that held open for fewer.
    #[test]
    fn byte_identical_rungs_fold_into_one_token() {
        let mut variant = sample();
        for _ in 0..4 {
            assert!(!variant.resp(Who::Alice, &response(OK_200)), "a rung appends no token");
        }
        assert_eq!(variant.alice, sample().alice);
        assert_transparent("self", &sample(), &variant, true); // does not panic
    }

    /// A repeat that is not the datagram — a re-composed 2xx — is a token of
    /// its own and fails the compare against a baseline that saw one.
    #[test]
    #[should_panic(expected = "TRANSPARENCY VIOLATION on the caller")]
    fn a_differing_repeat_is_its_own_token_and_fails_the_oracle() {
        let mut variant = sample();
        let recomposed = OK_200.replace("branch=z9hG4bK1", "branch=z9hG4bK1;rport");
        assert!(variant.resp(Who::Alice, &response(&recomposed)), "a differing datagram is a token");
        assert_eq!(variant.alice, vec!["RESP 200 cseq=1", "RESP 200 cseq=1"]);
        assert_transparent("self", &sample(), &variant, true);
    }

    /// The fold reads one emitter's stream: the same datagram again after
    /// another one intervened is not consecutive, and is a token.
    #[test]
    fn only_a_consecutive_repeat_folds() {
        let mut o = Observation::new();
        o.req(Who::Bob, &request(INVITE));
        o.req(Who::Bob, &request(ACK));
        assert!(o.req(Who::Bob, &request(INVITE)), "not consecutive, so not a rung");
        assert_eq!(o.bob.len(), 3);
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
