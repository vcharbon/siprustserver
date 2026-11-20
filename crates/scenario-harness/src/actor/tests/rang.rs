//! A caller rang once a provisional above 100 answered her INVITE — not when
//! she originated it, which already makes her leg `Early`. The abandon shapes
//! CANCEL on it, so the CANCEL meets a ringing callee (RFC 3261 §9.1).

use crate::actor::*;

fn fact(status: u16, cseq_method: &str) -> ResponseFact {
    ResponseFact {
        status,
        reason: String::new(),
        cseq_method: cseq_method.into(),
        body_len: 0,
        body_is_sdp: false,
        early_tag: None,
        typed: None,
    }
}

#[tokio::test]
async fn a_caller_rings_on_a_provisional_to_her_invite_only() {
    let obs = ObservedState::new();
    let now = tokio::time::Instant::now();
    obs.record(Observation::LegEarly { leg: "alice" }, now);
    obs.record(Observation::LegResponse { leg: "alice", fact: fact(100, "INVITE") }, now);
    obs.record(Observation::LegResponse { leg: "alice", fact: fact(180, "PRACK") }, now);
    assert!(obs.with_snapshot(|s| s.leg_at_least("alice", LegPhase::Early)));
    assert!(!obs.with_snapshot(|s| s.leg_rang("alice")), "originated, not rung");
    obs.record(Observation::LegResponse { leg: "alice", fact: fact(180, "INVITE") }, now);
    assert!(obs.with_snapshot(|s| s.leg_rang("alice")));
    assert!(!obs.with_snapshot(|s| s.leg_rang("bob")), "an absent leg never rang");
}

/// A 180 to her INVITE the loss model dropped on its way to the caller,
/// noted with what had reached her before it.
fn lost_ring(arrived_before: Vec<(u16, String)>) -> Observation {
    let message = crate::realcall::LostMessage {
        leg: None,
        inbound: true,
        method: None,
        status: Some(180),
        cseq_method: "INVITE".to_string(),
        sdp: false,
        reliable: false,
        to_tag: "bt2".to_string(),
        call_id: "c".to_string(),
        from_tag: "a".to_string(),
        cseq: 1,
        arrived_before,
    };
    Observation::Lost { leg: "alice", message }
}

/// Her ringing was lost unheard when the network dropped a provisional
/// before any had reached her, whenever the drop was folded in: a drop the
/// network noted after a delivered 180, folded before she read that 180, is
/// no lost ringing.
#[tokio::test]
async fn a_ring_is_lost_unheard_by_the_network_s_arrival_order() {
    let unheard = |arrived_before: Vec<(u16, String)>| {
        let obs = ObservedState::new();
        let now = tokio::time::Instant::now();
        obs.record(Observation::LegEarly { leg: "alice" }, now);
        obs.record(lost_ring(arrived_before), now);
        obs.record(Observation::LegResponse { leg: "alice", fact: fact(180, "INVITE") }, now);
        obs.with_snapshot(|s| s.leg("alice").ring_lost_unheard())
    };
    assert!(unheard(vec![(100, "INVITE".to_string())]), "dropped before any ringing arrived");
    assert!(!unheard(vec![(180, "INVITE".to_string())]), "dropped after a 180 arrived");
}
