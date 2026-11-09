//! The stuck criterion: a poll is stuck when its thread's slot stayed odd on
//! one sequence number while the thread spent more than the threshold of CPU.
//! Only CPU the thread spent inside that one poll is charged: a throttled,
//! starved, stopped or blocked thread spends none, work between polls runs at
//! an even sequence number, and wall time is never read.

use std::time::Duration;

/// One read of one thread: its slot's sequence number and its CPU time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Reading {
    pub seq: u64,
    pub cpu: Duration,
}

/// A poll seen in progress: its sequence number, and the thread's CPU time
/// at the first read that saw it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Watch {
    seq: u64,
    cpu_from: Duration,
}

/// What one read says of a thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Between polls.
    Idle,
    /// Inside a poll that has not spent the threshold yet; the watch carries
    /// to the next read.
    Running(Watch),
    /// Inside a poll that spent `spent` of CPU since the first read saw it,
    /// more than the threshold.
    Stuck { spent: Duration },
}

/// The verdict on `now`, given the watch the previous read left (`None`
/// after an idle read or before the first).
pub fn judge(previous: Option<Watch>, now: Reading, threshold: Duration) -> Verdict {
    if now.seq.is_multiple_of(2) {
        return Verdict::Idle;
    }
    match previous {
        Some(watch) if watch.seq == now.seq => {
            let spent = now.cpu.saturating_sub(watch.cpu_from);
            if spent > threshold {
                Verdict::Stuck { spent }
            } else {
                Verdict::Running(watch)
            }
        }
        _ => Verdict::Running(Watch { seq: now.seq, cpu_from: now.cpu }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: Duration = Duration::from_millis(10_000);

    /// The verdicts of successive reads, each fed the watch the one before
    /// left.
    fn verdicts(reads: &[(u64, u64)]) -> Vec<Verdict> {
        let mut watch = None;
        reads
            .iter()
            .map(|&(seq, cpu_ms)| {
                let reading = Reading { seq, cpu: Duration::from_millis(cpu_ms) };
                let verdict = judge(watch, reading, THRESHOLD);
                watch = match verdict {
                    Verdict::Running(w) => Some(w),
                    _ => None,
                };
                verdict
            })
            .collect()
    }

    fn stuck(reads: &[(u64, u64)]) -> bool {
        verdicts(reads).iter().any(|v| matches!(v, Verdict::Stuck { .. }))
    }

    #[test]
    fn one_poll_spending_past_the_threshold_is_stuck() {
        let reads = [(7, 0), (7, 1_000), (7, 5_000), (7, 10_001)];
        let last = verdicts(&reads).pop();
        assert_eq!(last, Some(Verdict::Stuck { spent: Duration::from_millis(10_001) }));
    }

    #[test]
    fn one_poll_at_the_threshold_is_not_stuck_yet() {
        assert!(!stuck(&[(7, 0), (7, 10_000)]));
    }

    #[test]
    fn a_starved_or_blocked_poll_spends_nothing_and_never_reads_stuck() {
        let reads: Vec<(u64, u64)> = (0..100).map(|_| (7, 42)).collect();
        assert!(!stuck(&reads), "a thread off the CPU inside one poll spends no CPU");
    }

    #[test]
    fn a_throttled_poll_reads_stuck_only_once_its_own_cpu_passes_the_threshold() {
        // A tenth of a CPU per read: a hundred reads to spend ten seconds.
        let reads: Vec<(u64, u64)> = (0..=100).map(|i| (7, i * 100)).collect();
        assert!(!stuck(&reads));
        let reads: Vec<(u64, u64)> = (0..=101).map(|i| (7, i * 100)).collect();
        assert!(stuck(&reads));
    }

    #[test]
    fn work_between_polls_is_never_charged() {
        let reads = [(8, 0), (8, 20_000), (10, 40_000), (12, 60_000)];
        assert_eq!(verdicts(&reads), vec![Verdict::Idle; 4]);
    }

    #[test]
    fn a_new_poll_at_each_read_starts_its_own_charge() {
        let reads = [(7, 0), (9, 9_000), (11, 18_000), (13, 27_000), (15, 36_000)];
        assert!(!stuck(&reads), "each poll spent under the threshold");
    }

    #[test]
    fn the_charge_starts_at_the_first_read_that_saw_the_poll() {
        // The thread spent 50 s before the poll began; only the poll's own
        // CPU counts.
        assert!(!stuck(&[(6, 50_000), (7, 50_000), (7, 59_000)]));
    }
}
