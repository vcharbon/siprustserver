//! A parked call armed after it met the other owner's copy (ADR-0055 §7, §8):
//! the copy taken whole carries the keepalive probe the other owner sent,
//! whose answer went to that owner. Arming the call never re-sends that probe
//! nor settles it as a timeout: the call stays up and probes on its interval.

use std::time::Duration;

use call::{CallModelState, DialogUsage, HostPort, RequestRecord, SentRequest};

use super::tests::{answered, body, stamped};
use super::{meet_refused_flush, FlushDirection};
use crate::router::materialise::arm;
use crate::router::test_support::node;
use crate::store::MaterialiseOrigin;

/// The keepalive probe the backup `w1` sent on the caller's dialog at `cseq`,
/// its Timer F ending at `deadline_ms`.
fn backups_probe(cseq: i64, deadline_ms: i64) -> RequestRecord {
    let sent = SentRequest {
        branch: format!("z9hG4bK-backup-probe-{cseq}"),
        image: b"OPTIONS sip:alice@192.0.2.10 SIP/2.0\r\n\r\n".to_vec(),
        destination: HostPort { host: "192.0.2.10".into(), port: 5060 },
    };
    RequestRecord {
        author: Some(call::KEEPALIVE_AUTHOR.into()),
        relayed_by: call::RelayOwner::new("w1", "w1"),
        ..RequestRecord::own("OPTIONS", cseq, sent, deadline_ms, DialogUsage::Session)
    }
}

/// Parked after a reboot, the call meets the backup's copy holding the
/// backup's probe and takes it whole; armed past that probe's Timer F, the
/// call stands, and it holds no probe of the other owner's.
#[tokio::test(start_paused = true)]
async fn a_parked_call_armed_past_the_backups_probe_stands() {
    let n = node("w0").await;
    let ctx = n.core.router_ctx();
    let live = stamped(answered(5, 1, CallModelState::Active), 4, 1);
    let r = live.call.call_ref.clone();
    let mut copy = answered(4, 2, CallModelState::Active);
    let d = copy.call.a_leg.dialogs.confirmed_mut().expect("the caller's dialog");
    d.our_cseq += 1;
    d.requests.push(backups_probe(d.our_cseq, 32_000));

    assert!(ctx.state.materialize_if_absent(live.clone(), MaterialiseOrigin::Reclaim));
    ctx.state.park(&r);
    meet_refused_flush(ctx, FlushDirection::Reverse, &r, &body(&copy), 0).await;
    sip_clock::testkit::settle().await;
    let met = ctx.state.peek(&r).expect("the call plays on");
    assert!(
        call::helpers::every_record(&met).any(|(_, _, r)| r.is_keepalive()),
        "the parked call took the backup's copy, its probe with it"
    );

    tokio::time::advance(Duration::from_secs(40)).await;
    {
        let _guard = ctx.state.lock(&r).await;
        assert!(arm(ctx, &r, None).await.is_some(), "the parked call is armed");
    }
    sip_clock::testkit::settle().await;
    tokio::time::advance(Duration::from_secs(1)).await;
    sip_clock::testkit::settle().await;
    let armed = ctx.state.peek(&r);
    assert_eq!(
        armed.as_ref().map(|c| c.state),
        Some(CallModelState::Active),
        "the backup's probe ends nothing here"
    );
    let armed = armed.expect("the call plays on");
    assert!(
        !call::helpers::every_record(&armed).any(|(_, _, r)| r.is_keepalive()),
        "the armed call holds no probe of the other owner's"
    );
}
