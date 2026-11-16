//! A caller whose ringing the network lost still abandons: the SUT's
//! unreliable 180 to her INVITE is dropped on its way to her, and her CANCEL,
//! timed from her ringing, goes out once the loss model reported the drop and
//! she heard the 100 (RFC 3261 §9.1). The call ends on an accepted ending,
//! never by a step timeout.

use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use loadgen::{CallTuning, Driver, DropDir, Reporter, ReporterCfg, ResultClass, TargetedDrop};

use crate::fake_rig::*;

/// `shape`'s calls, each losing the caller's first provisional to its INVITE.
async fn ringing_lost(base: u16, shape: &'static str) {
    let (h, b2bua, core, transport) = setup_fake(base).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let mut c = cfg(b2bua.addr, 4.0, 1, 4, 0x10_57);
    c.call.ring_delay = Duration::from_millis(300);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit: true,
        drop_nth: Some(TargetedDrop::provisional("INVITE", 1, DropDir::Inbound)),
    };
    Driver::new(c, vec![mix(shape, 1.0)], reporter.clone(), transport).run().await;
    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;

    let total = reporter.total_calls();
    assert!(total >= 4, "calls ran: {total}");
    let prom = reporter.render_prometheus();
    assert_eq!(reporter.count(shape, &ResultClass::Ok), total, "every call ok:\n{prom}");
    assert!(core.stats().dropped_in.load(std::sync::atomic::Ordering::Relaxed) >= total);
    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

#[tokio::test(start_paused = true)]
async fn an_abandon_whose_ringing_was_lost_still_cancels() {
    ringing_lost(7720, "abandon_ringing").await;
}

#[tokio::test(start_paused = true)]
async fn a_cancel_answer_crossing_whose_ringing_was_lost_still_ends() {
    ringing_lost(7730, "cancel_answer_crossing").await;
}
