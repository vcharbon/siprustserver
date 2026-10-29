//! A server transaction outlives its call (RFC 3261 §17.2.2): the callee's
//! 200 to the BYE is lost after the call ended, the SUT retransmits its BYE,
//! and the mux answers it again with the same 200. The repair is counted, on
//! the mux and on the BYE's arrow of the ladder, and is no orphan.

use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use b2bua_harness::settle_until;
use loadgen::{
    CallTuning, Canaries, Driver, DropDir, LoadRunMeta, Reporter, ReporterCfg, ResultClass,
    TargetedDrop,
};

use crate::fake_rig::*;

/// Every basic call loses bob's first 200 (BYE) before the wire; the call
/// is already over when the SUT's Timer E re-sends the BYE.
async fn bob_bye_answer_lost(base: u16, retransmit: bool) {
    let (h, b2bua, core, transport) = setup_fake(base).await;
    let reporter =
        Arc::new(Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 }));
    let mut c = cfg(b2bua.addr, 4.0, 2, 4, 0xB1E);
    c.default_tuning = CallTuning {
        drop_rate: 0.0,
        retransmit,
        drop_nth: Some(TargetedDrop::final_response("BYE", 1, DropDir::Outbound).on_leg("bob")),
    };
    Driver::new(c, vec![mix("basic_call", 1.0)], reporter.clone(), transport).run().await;
    settle_until(|| core.registry_size() == 0).await;
    h.advance(Duration::from_secs(40)).await;

    let total = reporter.total_calls();
    let prom = reporter.render_prometheus();
    assert!(total >= 4, "calls ran: {total}");
    assert_eq!(reporter.count("basic_call", &ResultClass::Ok), total, "every call ok:\n{prom}");
    let s = core.stats();
    assert_eq!(s.dropped_out.load(Relaxed), total, "one 200 (BYE) lost per call");
    assert_eq!(s.orphans_total(), 0, "no orphan: {:?}", s.samples());
    let sut = b2bua.metrics().prometheus_text();
    assert!(!sut.contains("kind=\"response_timeout\""), "the SUT's BYE completed:\n{sut}");
    assert_eq!(s.late_answered.load(Relaxed), total, "each retransmitted BYE answered again");
    assert!(
        core.render_prometheus().contains(&format!(
            "loadgen_mux_late_retransmission_answered_total{{stream=\"main\"}} {total}"
        )),
        "exported:\n{}",
        core.render_prometheus()
    );

    let idx = reporter.build_index(LoadRunMeta::default(), Canaries::default());
    let ladder = idx.ladders.iter().find(|l| l.scenario == "basic_call").expect("a ladder");
    let bye = ladder
        .arrows
        .iter()
        .find(|a| (a.from.as_str(), a.to.as_str(), a.label.as_str()) == ("SUT", "bob", "BYE"))
        .expect("bob's BYE arrow");
    assert_eq!(bye.late_answered, total, "on the BYE's arrow: {ladder:#?}");
    let elsewhere: u64 = ladder.arrows.iter().map(|a| a.late_answered).sum();
    assert_eq!(elsewhere, total, "and nowhere else");
    let window = reporter.tick(s.late_answered.load(Relaxed));
    assert_eq!(window.late_answered, total, "the headline's count, in the run's first window");

    settle_until(|| b2bua.is_reaped()).await;
    b2bua.assert_fully_reaped();
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_bye_retransmitted_after_the_call_is_answered_again() {
    bob_bye_answer_lost(7700, false).await;
}

#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_bye_retransmitted_after_the_call_is_answered_again_with_the_engine() {
    bob_bye_answer_lost(7710, true).await;
}

/// A raw SUT's initial INVITE to a callee endpoint, branch `z9hG4bK-rej`.
fn rejected_invite(method: &str, cseq: &str, branch: &str) -> Vec<u8> {
    format!(
        "{method} sip:bob@127.0.0.1:46401 SIP/2.0\r\nVia: SIP/2.0/UDP 127.0.0.1:46409;branch={branch}\r\n\
         Max-Forwards: 70\r\nCall-ID: cid-rej\r\nX-Loadgen-Id: tok-rej\r\n\
         From: <sip:sut@h>;tag=s1\r\nTo: <sip:bob@h>{}\r\nCSeq: {cseq}\r\n\
         Contact: <sip:sut@127.0.0.1:46409>\r\nContent-Length: 0\r\n\r\n",
        if method == "ACK" { ";tag=b1" } else { "" }
    )
    .into_bytes()
}

/// The callee rejects an initial INVITE with 486 and its call ends; the SUT
/// then retransmits the INVITE (the 486 lost): the mux answers it with the
/// same 486, not as a `released` orphan, and absorbs its ACK; an INVITE of
/// another transaction on the Call-ID stays `released`.
#[tokio::test(start_paused = true)]
#[ignore = "slow lane: loadgen"]
async fn a_retransmitted_rejected_invite_draws_its_486_and_its_ack_is_absorbed() {
    use loadgen::{CallRouting, Correlation, EndpointSpec, MuxCore, Role};
    use sip_net::{BindUdpOpts, SignalingNetwork};

    let sim = Arc::new(sip_net_testkit::SimulatedSignalingNetwork::new(1));
    let core = MuxCore::bind_on(
        sim.as_ref(),
        vec![EndpointSpec { addr: addr(46401), role: Role::Callee }],
        Correlation::header("X-Loadgen-Id"),
        64,
        8,
        RECV,
        sip_clock::Clock::test_at(0),
    )
    .await
    .unwrap();
    let sut = sim.bind_udp(BindUdpOpts::new(addr(46409), 64)).await.unwrap();
    let recv = || async {
        let p = tokio::time::timeout(Duration::from_secs(2), sut.recv()).await.ok().flatten();
        p.map(|p| p.raw.to_vec())
    };
    let invite = rejected_invite("INVITE", "1 INVITE", "z9hG4bK-rej");

    let net = core.network(CallRouting::new("tok-rej").leg(addr(46401), "bob"));
    let bob = net.bind_udp(BindUdpOpts::new(addr(46401), 16)).await.unwrap();
    sut.send_to(&invite, addr(46401)).await.unwrap();
    let got = tokio::time::timeout(RECV, bob.recv()).await.unwrap().unwrap();
    assert!(got.raw.starts_with(b"INVITE "));
    let busy = b"SIP/2.0 486 Busy Here\r\nVia: SIP/2.0/UDP 127.0.0.1:46409;branch=z9hG4bK-rej\r\n\
        Call-ID: cid-rej\r\nFrom: <sip:sut@h>;tag=s1\r\nTo: <sip:bob@h>;tag=b1\r\n\
        CSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
        .to_vec();
    bob.send_to(&busy, addr(46409)).await.unwrap();
    assert_eq!(recv().await, Some(busy.clone()), "the live 486");
    drop(bob);
    drop(net);

    sut.send_to(&invite, addr(46401)).await.unwrap();
    assert_eq!(recv().await, Some(busy), "the retransmitted INVITE draws the same 486");
    sut.send_to(&rejected_invite("ACK", "1 ACK", "z9hG4bK-rej"), addr(46401)).await.unwrap();
    assert_eq!(recv().await, None, "its ACK draws nothing");
    let s = core.stats();
    assert_eq!(s.late_answered.load(Relaxed), 1);
    assert_eq!(s.orphans_total(), 0, "neither is an orphan: {:?}", s.samples());

    sut.send_to(&rejected_invite("INVITE", "2 INVITE", "z9hG4bK-new"), addr(46401)).await.unwrap();
    assert_eq!(recv().await, None);
    assert_eq!(s.orphan_released.load(Relaxed), 1, "another transaction stays released");
}
