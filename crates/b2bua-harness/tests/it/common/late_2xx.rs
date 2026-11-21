//! A late 2xx to an INVITE whose client transaction owes it nothing (RFC 6026
//! §7.2): the peer that sent it, never ACKed, re-sends it on the §13.3.1.4
//! schedule; what this stack sent each side meanwhile, and its legs around
//! it, are what the scenarios read.

use std::net::SocketAddr;
use std::time::Duration;

use b2bua::config::B2buaConfig;
use scenario_harness::{Agent, Harness};

/// The gaps between an un-ACKed UAS's sends of its 2xx (RFC 3261 §13.3.1.4):
/// T1 doubling to T2, the last copy inside 64·T1 of the first.
pub const LADDER_MS: [u64; 10] =
    [500, 1_000, 2_000, 4_000, 4_000, 4_000, 4_000, 4_000, 4_000, 4_000];

/// How long after its first send the UAS gives the 2xx up and clears the
/// dialog with a BYE (§13.3.1.4: 64·T1).
pub const GIVE_UP: Duration = Duration::from_secs(32);

/// How long after its last copy the SUT has read it.
const SETTLE: Duration = Duration::from_millis(5);

/// The SUT's configuration in these scenarios: no keepalive inside the
/// window, and the message ring on, so a leg records each distinct message
/// it reads.
pub fn reading_every_message(c: &mut B2buaConfig) {
    c.keepalive_interval_sec = 3_600;
    c.cdr.message_ring = 32;
}

/// `peer`, which just sent the 2xx `raw` to `to`, sends it again on each rung
/// of [`LADDER_MS`], draining what it is sent on the way; returns once the
/// last copy was read, with the time left until [`GIVE_UP`].
pub async fn repeat_on_ladder(h: &Harness, peer: &Agent, raw: &[u8], to: SocketAddr) -> Duration {
    for gap_ms in LADDER_MS {
        h.advance(Duration::from_millis(gap_ms)).await;
        peer.drain().await;
        peer.try_send_datagram(raw, to).await.expect("its copy leaves");
    }
    h.advance(SETTLE).await;
    peer.drain().await;
    GIVE_UP - Duration::from_millis(LADDER_MS.iter().sum()) - SETTLE
}

/// The start lines of what the SUT at `b2bua` sent `to` in the wire entries
/// past the first `from`.
pub fn sent_to(h: &Harness, b2bua: SocketAddr, to: SocketAddr, from: usize) -> Vec<String> {
    h.wire_entries()
        .into_iter()
        .skip(from)
        .filter(|e| e.from == b2bua && e.to == to)
        .map(|e| String::from_utf8_lossy(&e.raw).lines().next().unwrap_or_default().to_string())
        .collect()
}

/// The raw bytes of the last 2xx `from` sent `to` on the wire.
pub fn last_2xx(h: &Harness, from: SocketAddr, to: SocketAddr) -> Vec<u8> {
    h.wire_entries()
        .into_iter()
        .filter(|e| e.from == from && e.to == to && e.raw.starts_with(b"SIP/2.0 2"))
        .last()
        .expect("a 2xx on the wire")
        .raw
}

/// Every leg of `call`, the caller's first: its dialogs, its session and the
/// messages it read.
pub fn legs(call: &call::Call) -> Vec<call::Leg> {
    std::iter::once(&call.a_leg).chain(&call.b_legs).cloned().collect()
}

/// Assert each leg of `now` is the leg `held` recorded: nothing read since,
/// on any of its dialogs or its session. A failure names the leg and the
/// parts of it that moved, not the whole record.
#[track_caller]
pub fn assert_legs_unchanged(held: &[call::Leg], now: &call::Call) {
    let now = legs(now);
    assert_eq!(now.len(), held.len(), "the call's legs");
    for (was, is) in held.iter().zip(&now) {
        assert!(was == is, "leg {} read the late 2xx: {:?}", was.leg_id, moved(was, is));
    }
}

/// The parts of leg `was` that differ in `is`.
fn moved(was: &call::Leg, is: &call::Leg) -> Vec<String> {
    let mut parts = Vec::new();
    for (a, b) in was.dialogs.iter().zip(is.dialogs.iter()) {
        let fields = [
            ("state", a.state == b.state),
            ("cseqs", (a.our_cseq, a.peer_cseq) == (b.our_cseq, b.peer_cseq)),
            ("sent_invite", a.sent_invite == b.sent_invite),
            ("untaken_invite", a.untaken_invite == b.untaken_invite),
            ("answered_invite", a.answered_invite == b.answered_invite),
            ("owed_answer", a.owed_answer == b.owed_answer),
            ("requests", a.requests == b.requests),
        ];
        let differ: Vec<&str> = fields.iter().filter(|(_, same)| !same).map(|(f, _)| *f).collect();
        if a != b {
            parts.push(format!("dialog {} {differ:?}", a.our_tag));
        }
    }
    if was.sdp_session != is.sdp_session {
        parts.push("session".into());
    }
    if was.messages != is.messages {
        parts.push("messages".into());
    }
    if parts.is_empty() {
        parts.push("another field".into());
    }
    parts
}
