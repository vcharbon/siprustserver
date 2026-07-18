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

impl CseqDeviation {
    pub fn new(pattern: CseqPattern) -> Self {
        CseqDeviation { pattern, step: 0, offset_applied: false }
    }

    /// Whether the underlying pattern is the identity.
    pub fn is_identity(&self) -> bool {
        self.pattern.is_identity()
    }

    /// The CSeq to emit for the NEXT in-dialog request, given the dialog's
    /// current running `local_cseq` (the last emitted number, or the INVITE's).
    /// Advances the internal step. The dialog then sets its `local_cseq` to the
    /// returned value, so subsequent natural increments continue from here.
    pub fn next_cseq(&mut self, local_cseq: u32) -> u32 {
        let step = self.step;
        self.step += 1;
        let offset = if self.offset_applied {
            0
        } else {
            self.offset_applied = true;
            self.pattern.offset
        };
        match self.pattern.op_at(step) {
            // Reuse emits the previous number and does NOT advance (the dialog's
            // local_cseq is set back to this same value, so the next natural
            // request continues from it).
            Some(CseqOp::Reuse) => local_cseq,
            Some(CseqOp::Jump { by }) => shift(local_cseq, 1 + by + offset),
            None => shift(local_cseq, 1 + offset),
        }
    }
}

/// Apply a signed delta to a CSeq, clamped to the u32 range.
fn shift(base: u32, delta: i64) -> u32 {
    (base as i64 + delta).clamp(0, u32::MAX as i64) as u32
}

/// A stack protocol automatic a [`DelayedAutomatic`] can hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Automatic {
    /// The UAC's automatic ACK to a 2xx final (RFC 3261 §13.2.2.4).
    AckTo2xx,
}

/// Hold a stack automatic for a declared duration before it fires. v1 delays
/// only WHEN the automatic fires — its content is unchanged (the U3 boundary:
/// automatics take no template).
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
}
