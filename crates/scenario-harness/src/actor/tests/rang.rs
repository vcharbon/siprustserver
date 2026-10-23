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
