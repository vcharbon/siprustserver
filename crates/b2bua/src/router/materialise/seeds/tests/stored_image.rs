//! A seed reads its image with no limit, no gate and no setting.

use super::*;
use crate::router::materialise::rejected_dialled_seeds;

/// `image` holding lines the strict parser refuses: one past every length
/// limit, and a stated value the numeric gate refuses.
fn holding_refused_lines(image: &[u8]) -> Vec<u8> {
    use sip_message::{CustomParser, SipParser};
    let text = std::str::from_utf8(image).expect("text");
    let held = format!("X-Long: {}\r\nExpires: soon\r\nContent-Length:", "a".repeat(9_000));
    let refused = text.replace("Content-Length:", &held).into_bytes();
    assert!(CustomParser::new().parse(&refused).is_err(), "the strict parser refuses it");
    refused
}

/// Every seed forms from a stored image holding a line the strict parser
/// refuses: a stored read has no limit, no gate and no setting.
#[test]
fn every_seed_forms_from_an_image_the_strict_parser_refuses() {
    let branch_of = |seeds: &[TxnSeed], pick: fn(&TxnSeed) -> bool| seeds.iter().any(pick);
    let handle = |branch: &str, cseq: u32| {
        let mut sent = b_handle(branch, cseq);
        sent.image = holding_refused_lines(&sent.image);
        sent
    };

    // A client INVITE, and the provisional a server INVITE's seed carries.
    let mut call = call_with(LegState::Early, b_leg(LegState::Early, Some(handle(B_BRANCH, 1))));
    ring_a_leg(&mut call, "svca");
    let ringing = format!(
        "SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch={A_BRANCH}\r\n\
         From: <sip:alice@10.0.0.1>;tag=atag\r\nTo: <sip:bob@10.0.0.9>;tag=svca\r\n\
         Call-ID: acid@alice\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    *call.a_leg.invite_provisional_mut() = Some(call::RetainedEmission::on_trigger(
        holding_refused_lines(ringing.as_bytes()),
        ("10.0.0.1".to_string(), 5060),
        call::Repeated::response("INVITE", 180),
    ));
    let seeds = seeds_for(&call, 0);
    assert_eq!(client_branches(&seeds), vec![B_BRANCH.to_string()], "{seeds:?}");
    assert!(
        branch_of(
            &seeds,
            |s| matches!(s, TxnSeed::ServerInvite { provisional: Some(p), .. } if p.status() == 180)
        ),
        "{seeds:?}"
    );

    // The dialled INVITE that took a non-2xx final, and the caller's kept final.
    let mut call =
        call_with(LegState::Terminated, b_leg(LegState::Trying, Some(handle(B_BRANCH, 1))));
    call::helpers::record_dialled_invite_final(&mut call, "b-1", 1, 486, 0);
    let dialled = rejected_dialled_seeds(&call, 0);
    assert!(matches!(dialled.as_slice(), [TxnSeed::ClientRejected { .. }]), "{dialled:?}");
    let final_486 = format!(
        "SIP/2.0 486 Busy Here\r\nVia: SIP/2.0/UDP 10.0.0.1:5060;branch={A_BRANCH}\r\n\
         From: <sip:alice@10.0.0.1>;tag=atag\r\nTo: <sip:bob@10.0.0.9>;tag=own\r\n\
         Call-ID: acid@alice\r\nCSeq: 1 INVITE\r\nContent-Length: 0\r\n\r\n"
    );
    *call.a_leg.invite_final_sent_mut() = Some(486);
    *call.a_leg.invite_rejection_mut() = Some(call::RetainedEmission::on_trigger(
        holding_refused_lines(final_486.as_bytes()),
        ("10.0.0.1".to_string(), 5060),
        call::Repeated::response("INVITE", 486),
    ));
    assert!(
        branch_of(&seeds_for(&call, 0), |s| matches!(s, TxnSeed::ServerRejected { .. })),
        "the kept final"
    );

    // A client non-INVITE, an Accepted server INVITE, a Completed non-INVITE.
    let mut leg = b_leg(LegState::Confirmed, Some(b_handle(B_BRANCH, 1)));
    let sent = call::SentRequest {
        branch: "z9hG4bK-own-info".into(),
        image: holding_refused_lines(&b_handle("z9hG4bK-own-info", 2).image),
        destination: HostPort { host: "10.0.0.9".into(), port: 5080 },
    };
    confirmed(&mut leg).requests.push(RequestRecord::own(
        "INFO",
        2,
        sent,
        30_000,
        Default::default(),
    ));
    retain_reinvite_2xx(&mut leg, RELAYED_BRANCH);
    let retained = confirmed(&mut leg).answered_invite.as_mut().expect("the retained 2xx");
    let answered_2xx = retained.invite.as_mut().and_then(|x| x.answered_2xx.as_mut());
    let answered_2xx = answered_2xx.expect("the retained 2xx");
    let (bytes, (host, port)) = answered_2xx.emission.wire();
    let (bytes, dest) = (holding_refused_lines(bytes), (host.to_string(), port));
    answered_2xx.emission =
        call::RetainedEmission::on_trigger(bytes, dest, call::Repeated::response("INVITE", 200));
    let mut call = call_with(LegState::Confirmed, leg);
    let info_200 = "SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 10.0.0.2:5070;branch=z9hG4bK-info\r\n\
         From: <sip:bob@10.0.0.2>;tag=btag\r\nTo: <sip:svc@10.0.0.9>;tag=svc\r\n\
         Call-ID: bcid@x\r\nCSeq: 9 INFO\r\nContent-Length: 0\r\n\r\n";
    call.answered.push(call::AnsweredRequest {
        method: "INFO".into(),
        branch: "z9hG4bK-info".into(),
        response: call::RetainedEmission::on_trigger(
            holding_refused_lines(info_200.as_bytes()),
            ("10.0.0.2".to_string(), 5070),
            call::Repeated::response("INFO", 200),
        ),
        until_ms: 30_000,
    });
    let seeds = seeds_for(&call, 0);
    assert!(branch_of(&seeds, |s| matches!(s, TxnSeed::ClientNonInvite { .. })), "{seeds:?}");
    assert!(branch_of(&seeds, |s| matches!(s, TxnSeed::ServerAccepted { .. })), "{seeds:?}");
    let answered = answered_seeds(&call, 0);
    assert!(
        matches!(answered.as_slice(), [TxnSeed::ServerNonInviteAnswered { .. }]),
        "{answered:?}"
    );
}
