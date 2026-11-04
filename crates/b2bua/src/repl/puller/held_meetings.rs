//! The copies a stream's batch refused, held until the batch is read whole
//! (ADR-0055 §7). A flow delivers one owner's writes of a call in order, each
//! a descendant of the one before, and a link that heals delivers every write
//! it held as one batch, its frames back to back. The router meets only the
//! latest refused copy of each call of a batch — read whole at the server's
//! catch-up `Noop`, at the end of the stream, or once the stream stays quiet
//! for [`BATCH_QUIET`]: two copies that diverged end the call from the other
//! owner's latest state, never from one it already left behind.

use std::sync::Mutex;
use std::time::Duration;

/// How long a stream stays quiet after a frame before the batch it carried
/// counts as read whole: well past the delivery spacing of frames a link
/// releases at once, well under any SIP timer a meeting's outcome races.
pub(super) const BATCH_QUIET: Duration = Duration::from_millis(20);

use crate::router::ReplCommand;

/// The latest refused copy of each call the current batch carried, in the
/// order of each call's latest refusal.
#[derive(Default)]
pub(super) struct HeldMeetings(Mutex<Vec<(String, ReplCommand)>>);

impl HeldMeetings {
    /// Hold `cmd`, a refused copy of `call_ref`, in place of any earlier one.
    pub(super) fn hold(&self, call_ref: &str, cmd: ReplCommand) {
        let mut held = self.locked();
        held.retain(|(r, _)| r != call_ref);
        held.push((call_ref.to_string(), cmd));
    }

    /// Whether no copy is held.
    pub(super) fn is_empty(&self) -> bool {
        self.locked().is_empty()
    }

    /// The copy held for `call_ref`, taken out: a later frame of the call
    /// rides up after it.
    pub(super) fn take(&self, call_ref: &str) -> Option<ReplCommand> {
        let mut held = self.locked();
        let mut taken = None;
        for (r, cmd) in std::mem::take(&mut *held) {
            if taken.is_none() && r == call_ref {
                taken = Some(cmd);
            } else {
                held.push((r, cmd));
            }
        }
        taken
    }

    /// Every copy held, taken out in order: the batch is read whole.
    pub(super) fn take_all(&self) -> Vec<ReplCommand> {
        std::mem::take(&mut *self.locked()).into_iter().map(|(_, cmd)| cmd).collect()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, Vec<(String, ReplCommand)>> {
        crate::poison::locked(&self.0, crate::poison::PULLER_HELD_MEETINGS)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn refused(call_ref: &str, tag: u8) -> ReplCommand {
        let body: Arc<[u8]> = Arc::from(vec![tag]);
        ReplCommand::ReverseFlushRefused { call_ref: call_ref.into(), body, origin_now_ms: 0 }
    }

    fn tag(cmd: &ReplCommand) -> u8 {
        match cmd {
            ReplCommand::ReverseFlushRefused { body, .. } => body[0],
            other => panic!("not a refused copy: {other:?}"),
        }
    }

    /// A call's later refused copy replaces its earlier one.
    #[test]
    fn the_latest_refused_copy_of_each_call_is_held() {
        let held = HeldMeetings::default();
        held.hold("a", refused("a", 1));
        held.hold("b", refused("b", 1));
        held.hold("a", refused("a", 2));
        assert_eq!(held.take("b").as_ref().map(tag), Some(1));
        let rest: Vec<u8> = held.take_all().iter().map(tag).collect();
        assert_eq!(rest, vec![2]);
        assert!(held.take_all().is_empty());
    }
}
