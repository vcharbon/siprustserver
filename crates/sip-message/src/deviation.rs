//! Declared deviations for captured-anomaly replay — data-only (no closures) so
//! a scenario file can carry them; the scenario harness applies them. A CSeq
//! anomaly replays RELATIVE to the stack's base (never a frozen absolute
//! number), and a protocol automatic can be held for a declared duration.

/// One operation in a [`CseqPattern`], applied at a declared step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CseqOp {
    /// Add `by` to the running CSeq at this step; every subsequent request
    /// continues from the jumped value (relative to the stack's base).
    Jump { by: i64 },
    /// Emit the SAME CSeq number as the previous in-dialog request (a reuse —
    /// deliberately non-compliant, RFC 3261 §12.2.1.1).
    Reuse,
}

/// A [`CseqOp`] bound to a 0-based step index (the count of in-dialog requests a
/// dialog originates).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CseqOpAt {
    pub at: usize,
    pub op: CseqOp,
}

/// A declared RELATIVE CSeq pattern for a dialog's outbound in-dialog requests.
/// The empty pattern is the identity (the stack's base numbering is
/// authoritative). Data-only so a scenario file can carry it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CseqPattern {
    /// A constant shift added to the dialog's base CSeq before its FIRST deviated
    /// in-dialog request (all subsequent numbers inherit it). `0` = none.
    pub offset: i64,
    /// Per-step operations, by 0-based step index.
    pub ops: Vec<CseqOpAt>,
}

impl CseqPattern {
    /// A pattern with just a constant offset.
    pub fn with_offset(offset: i64) -> Self {
        CseqPattern { offset, ops: Vec::new() }
    }

    /// The operation declared at 0-based `step`, if any.
    pub fn op_at(&self, step: usize) -> Option<CseqOp> {
        self.ops.iter().find(|o| o.at == step).map(|o| o.op)
    }

    /// Whether this pattern is the identity (no deviation declared).
    pub fn is_identity(&self) -> bool {
        self.offset == 0 && self.ops.is_empty()
    }
}

/// Applies a [`CseqPattern`] across a dialog's successive in-dialog requests,
/// tracking the step index and the one-time offset application.
#[derive(Debug, Clone)]
pub struct CseqDeviation {
    pattern: CseqPattern,
    step: usize,
    offset_applied: bool,
}

/// The maximum legal CSeq sequence number (RFC 3261 §8.1.1.5: a CSeq is a 32-bit
/// unsigned integer that MUST be less than 2^31). Valid emitted values are
/// `[1, MAX_CSEQ]` — a fresh in-dialog request is never 0.
const MAX_CSEQ: i128 = i32::MAX as i128;

impl CseqDeviation {
    /// Build the applier, rejecting a malformed pattern loudly: at most one op
    /// per step (a second op at the same step would be silently dead).
    pub fn new(pattern: CseqPattern) -> Self {
        let mut seen = std::collections::BTreeSet::new();
        for o in &pattern.ops {
            if !seen.insert(o.at) {
                panic!(
                    "CseqPattern declares more than one op at step {} — a step carries at most one operation",
                    o.at
                );
            }
        }
        CseqDeviation { pattern, step: 0, offset_applied: false }
    }

    /// Whether the underlying pattern is the identity.
    pub fn is_identity(&self) -> bool {
        self.pattern.is_identity()
    }

    /// The ops declared at steps this deviation has NOT yet reached — a
    /// scenario declared them but the dialog originated too few in-dialog
    /// requests to consume them.
    pub fn unconsumed_ops(&self) -> Vec<CseqOpAt> {
        self.pattern.ops.iter().copied().filter(|o| o.at >= self.step).collect()
    }

    /// The CSeq to emit for the NEXT in-dialog request, given the dialog's
    /// current running `local_cseq` (the last emitted number, or the INVITE's).
    /// Advances the internal step. The dialog then sets its `local_cseq` to the
    /// returned value, so subsequent natural increments continue from here.
    ///
    /// Panics if the pattern's arithmetic leaves the legal CSeq range
    /// `[1, MAX_CSEQ]` (a clamp would put a bogus number on the wire silently),
    /// naming the offending step, op, and computed value.
    pub fn next_cseq(&mut self, local_cseq: u32) -> u32 {
        let step = self.step;
        self.step += 1;
        // The offset folds into the running baseline ONCE, regardless of which
        // op consumes the first deviated step (a Reuse then reuses the
        // offset-adjusted number). i128 keeps every intermediate overflow-free.
        let base: i128 = local_cseq as i128
            + if self.offset_applied {
                0
            } else {
                self.offset_applied = true;
                self.pattern.offset as i128
            };
        let op = self.pattern.op_at(step);
        let emit: i128 = match op {
            Some(CseqOp::Reuse) => base,
            Some(CseqOp::Jump { by }) => base + 1 + by as i128,
            None => base + 1,
        };
        if !(1..=MAX_CSEQ).contains(&emit) {
            panic!(
                "CSeq deviation produced out-of-range value {emit} at step {step} (op {op:?}) — \
                 a CSeq must be in [1, {MAX_CSEQ}] (RFC 3261 §8.1.1.5)"
            );
        }
        emit as u32
    }
}

/// A stack protocol automatic a [`DelayedAutomatic`] can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automatic {
    /// The UAC's automatic ACK to a 2xx final (RFC 3261 §13.2.2.4).
    AckTo2xx,
}

/// Hold a stack automatic for a declared duration before it fires. This delays
/// only WHEN the automatic fires; its content is unchanged (the stack emits the
/// automatic, never a template).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelayedAutomatic {
    pub which: Automatic,
    pub delay_ms: u64,
}

impl DelayedAutomatic {
    /// Hold the automatic ACK to a 2xx for `delay_ms` milliseconds.
    pub fn ack_after(delay_ms: u64) -> Self {
        DelayedAutomatic { which: Automatic::AckTo2xx, delay_ms }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_pattern_is_natural_increment() {
        let mut dev = CseqDeviation::new(CseqPattern::default());
        // INVITE was 1; successive in-dialog requests: 2, 3, 4.
        assert_eq!(dev.next_cseq(1), 2);
        assert_eq!(dev.next_cseq(2), 3);
        assert_eq!(dev.next_cseq(3), 4);
    }

    #[test]
    fn jump_shifts_this_and_subsequent_steps() {
        // Jump by +48 at step 0: 2+48 = 50, then continue 51, 52.
        let pat = CseqPattern { offset: 0, ops: vec![CseqOpAt { at: 0, op: CseqOp::Jump { by: 48 } }] };
        let mut dev = CseqDeviation::new(pat);
        let c0 = dev.next_cseq(1);
        assert_eq!(c0, 50);
        let c1 = dev.next_cseq(c0);
        assert_eq!(c1, 51);
        let c2 = dev.next_cseq(c1);
        assert_eq!(c2, 52);
    }

    #[test]
    fn reuse_emits_previous_number_then_continues() {
        // step 0: 2, step 1: reuse 2, step 2: continue to 3.
        let pat = CseqPattern { offset: 0, ops: vec![CseqOpAt { at: 1, op: CseqOp::Reuse }] };
        let mut dev = CseqDeviation::new(pat);
        let c0 = dev.next_cseq(1);
        assert_eq!(c0, 2);
        let c1 = dev.next_cseq(c0);
        assert_eq!(c1, 2, "reuse emits the previous number");
        let c2 = dev.next_cseq(c1);
        assert_eq!(c2, 3, "continues from the reused number");
    }

    #[test]
    fn offset_applied_once_and_inherited() {
        let mut dev = CseqDeviation::new(CseqPattern::with_offset(100));
        let c0 = dev.next_cseq(1);
        assert_eq!(c0, 102, "2 + offset 100");
        let c1 = dev.next_cseq(c0);
        assert_eq!(c1, 103, "offset not re-applied; inherited via the running value");
    }

    #[test]
    fn offset_survives_a_reuse_at_step_0() {
        // The offset folds into the baseline even though a Reuse consumes step 0;
        // step 1 continues from the offset-adjusted number.
        let pat = CseqPattern { offset: 100, ops: vec![CseqOpAt { at: 0, op: CseqOp::Reuse }] };
        let mut dev = CseqDeviation::new(pat);
        let c0 = dev.next_cseq(1);
        assert_eq!(c0, 101, "reuse of the offset-adjusted baseline (1 + 100)");
        let c1 = dev.next_cseq(c0);
        assert_eq!(c1, 102, "offset-shifted continuation");
    }

    #[test]
    fn offset_and_jump_interplay() {
        let pat = CseqPattern { offset: 10, ops: vec![CseqOpAt { at: 0, op: CseqOp::Jump { by: 5 } }] };
        let mut dev = CseqDeviation::new(pat);
        let c0 = dev.next_cseq(1);
        assert_eq!(c0, 17, "1 + offset 10 + 1 + jump 5");
        let c1 = dev.next_cseq(c0);
        assert_eq!(c1, 18);
    }

    #[test]
    #[should_panic(expected = "out-of-range")]
    fn negative_result_is_rejected_loudly() {
        let mut dev = CseqDeviation::new(CseqPattern::with_offset(-5));
        // base = 1 - 5 = -4, +1 = -3 → below 1 → reject (never clamp to 0).
        let _ = dev.next_cseq(1);
    }

    #[test]
    #[should_panic(expected = "out-of-range")]
    fn over_max_is_rejected_loudly() {
        let pat = CseqPattern {
            offset: 0,
            ops: vec![CseqOpAt { at: 0, op: CseqOp::Jump { by: i32::MAX as i64 } }],
        };
        let mut dev = CseqDeviation::new(pat);
        // 1 + 1 + 2147483647 = 2147483649 > 2^31-1 → reject.
        let _ = dev.next_cseq(1);
    }

    #[test]
    #[should_panic(expected = "out-of-range")]
    fn jump_i64_max_rejects_without_overflow() {
        let pat = CseqPattern {
            offset: 0,
            ops: vec![CseqOpAt { at: 0, op: CseqOp::Jump { by: i64::MAX } }],
        };
        let mut dev = CseqDeviation::new(pat);
        let _ = dev.next_cseq(1); // i128 math → no overflow, a clean range reject.
    }

    #[test]
    #[should_panic(expected = "more than one op at step")]
    fn duplicate_at_is_rejected_loudly() {
        let pat = CseqPattern {
            offset: 0,
            ops: vec![
                CseqOpAt { at: 0, op: CseqOp::Reuse },
                CseqOpAt { at: 0, op: CseqOp::Jump { by: 1 } },
            ],
        };
        let _ = CseqDeviation::new(pat);
    }

    #[test]
    fn unconsumed_ops_are_reported() {
        let pat = CseqPattern {
            offset: 0,
            ops: vec![
                CseqOpAt { at: 0, op: CseqOp::Jump { by: 1 } },
                CseqOpAt { at: 5, op: CseqOp::Reuse },
            ],
        };
        let mut dev = CseqDeviation::new(pat);
        let _ = dev.next_cseq(1); // consumes step 0.
        let un = dev.unconsumed_ops();
        assert_eq!(un.len(), 1);
        assert_eq!(un[0].at, 5, "the op at a step never reached is reported");
    }
}
