//! The pre-ingress hook run on the receive pump, with a panic inside it
//! contained to the one datagram (ADR-0051 §5): a hook that panics accepts
//! the datagram and is counted, so the pump keeps receiving and the consumer
//! sees the datagram as it would without a hook. Sound because a hook judges
//! borrowed bytes and returns a fresh verdict.

use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::Ordering;

use crate::net::Counters;
use crate::types::{PreIngressAction, PreIngressHook};

/// The verdict of `hook` on `raw` from `src` at queue depth `depth`:
/// [`PreIngressAction::Accept`] with no hook, or when the hook panics (counted
/// in `pre_ingress_panics`).
pub fn judge(
    hook: Option<&PreIngressHook>,
    raw: &[u8],
    src: SocketAddr,
    depth: usize,
    counters: &Counters,
) -> PreIngressAction {
    let Some(hook) = hook else { return PreIngressAction::Accept };
    catch_unwind(AssertUnwindSafe(|| hook(raw, src, depth))).unwrap_or_else(|_payload| {
        counters.pre_ingress_panics.fetch_add(1, Ordering::Relaxed);
        PreIngressAction::Accept
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn src() -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 1], 5060))
    }

    #[test]
    fn a_hook_that_panics_accepts_the_datagram_and_is_counted() {
        let counters = Counters::default();
        let hook: PreIngressHook = Arc::new(|_, _, _| panic!("probe: hook panic"));
        let action = judge(Some(&hook), b"INVITE", src(), 0, &counters);
        assert_eq!(action, PreIngressAction::Accept);
        assert_eq!(counters.snapshot().pre_ingress_panics, 1);
    }

    #[test]
    fn a_hook_verdict_is_returned_as_given() {
        let counters = Counters::default();
        let hook: PreIngressHook = Arc::new(|_, _, _| PreIngressAction::Drop);
        assert_eq!(judge(Some(&hook), b"INVITE", src(), 0, &counters), PreIngressAction::Drop);
        assert_eq!(counters.snapshot().pre_ingress_panics, 0);
    }
}
