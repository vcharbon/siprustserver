//! The adopted-`p` rules of the apply gate (ADR-0031, "an adopted counter is
//! not the body it counts"): the Forward exception at the Element's own
//! `(p,b)` and the Reverse refusal.

use std::sync::Arc;

use repl_net::frame::{Op, Partition};

use super::{backup_puller, body_at, forward};
use super::{ApplyMode, ApplyOutcome};

/// `body_at` an answered live call, its `p` adopted without the authority's
/// body when `adopted`.
fn adopted_body(adopted: bool) -> Arc<[u8]> {
    use call::CallBodyCodec;
    let codec = call::MsgpackCodec::new();
    let mut call =
        codec.decode(&body_at(Some(200), call::CallModelState::Active)).expect("decodes");
    call.topology.as_mut().expect("a proxied call").gen_adopted = adopted;
    Arc::from(codec.encode(&call))
}

/// At the Element's own `(p,b)`, the authority's call replaces an Element
/// whose `p` the acting backup adopted without the authority's body; over an
/// Element holding that version's body it is a re-delivery.
#[tokio::test]
async fn the_authoritys_call_replaces_an_element_whose_p_was_adopted() {
    for (marked, expected) in [(true, ApplyOutcome::Applied), (false, ApplyOutcome::Dominated)] {
        let (p, _, _, _) = backup_puller(crate::config::CapacityConfig::default());
        assert_eq!(forward(&p, (6, 3), adopted_body(marked)).await, ApplyOutcome::Applied);
        assert_eq!(forward(&p, (6, 3), adopted_body(false)).await, expected, "marked {marked}");
    }
}

/// A reverse flush whose `p` the backup adopted without our body is no
/// version untouched since it branched: our store keeps our own call, and the
/// body rides up to the fold.
#[tokio::test]
async fn a_reverse_flush_on_an_adopted_p_never_replaces_our_call() {
    for (marked, expected) in [(true, ApplyOutcome::Refused), (false, ApplyOutcome::Applied)] {
        let (p, _, _, _) = backup_puller(crate::config::CapacityConfig::default());
        let reverse = |body| {
            p.apply_to_store(
                Op::Put,
                Partition::Pri,
                "w0|split|t",
                6,
                3,
                60_000,
                0,
                &[],
                Some(body),
                false,
                None,
                ApplyMode::Reverse,
            )
        };
        assert_eq!(reverse(adopted_body(false)).await, ApplyOutcome::Applied);
        let bump = p.apply_to_store(
            Op::Put,
            Partition::Pri,
            "w0|split|t",
            6,
            4,
            60_000,
            0,
            &[],
            Some(adopted_body(marked)),
            false,
            None,
            ApplyMode::Reverse,
        );
        assert_eq!(bump.await, expected, "marked {marked}");
    }
}
