//! The store envelope: the body round trip, the owners a turn reads, and
//! the meeting merge of the vector.

use call::{MsgpackCodec, RelayOwner};

use super::*;

fn call() -> Call {
    let config = crate::config::B2buaConfig { self_ordinal: "w0".into(), ..Default::default() };
    let invite = crate::router::test_support::invite("w0", "w1", "envelope");
    let mut ids = sip_ids::TurnIds::fixed(1);
    crate::initial_invite::build_initial_call(
        &invite,
        crate::router::test_support::src(),
        &config,
        &mut ids,
        0,
    )
}

/// A body carries the call and its envelope, or the call alone.
#[test]
fn a_body_round_trips_the_call_and_its_envelope() {
    let codec = MsgpackCodec::new();
    let mut topology = CallTopology::new("w0", "w1", 4, 2);
    topology.stamp_gen = 3;
    let body = encode(&codec, Some(&topology), &call()).expect("encodes");
    assert_eq!(decode(&codec, &body).expect("decodes"), Enveloped::new(call(), Some(topology)));
    let bare = encode(&codec, None, &call()).expect("encodes");
    assert_eq!(decode(&codec, &bare).expect("decodes"), Enveloped::new(call(), None));
}

/// A body that is no envelope, or none at all, is refused.
#[test]
fn a_bare_call_body_or_an_empty_one_is_refused() {
    let codec = MsgpackCodec::new();
    let bare = call::CallBodyCodec::encode(&codec, &call()).expect("encodes");
    assert!(decode(&codec, &bare).is_err());
    assert!(matches!(decode(&codec, &[]), Err(CallDecodeError::Empty)));
}

fn on(node: &str, topology: Option<&CallTopology>) -> Owners {
    owners(&RelayOwner::new(node, "boot-1"), topology)
}

#[test]
fn the_primary_states_the_calls_backup() {
    let t = CallTopology::new("w0", "w1", 1, 0);
    assert_eq!(on("w0", Some(&t)).other.as_deref(), Some("w1"));
    assert_eq!(on("w0", Some(&t)).this, RelayOwner::new("w0", "boot-1"));
}

#[test]
fn a_copy_on_the_backup_states_the_primary() {
    let t = CallTopology::new("w0", "w1", 1, 0);
    assert_eq!(on("w1", Some(&t)).other.as_deref(), Some("w0"));
}

#[test]
fn a_call_with_one_owner_states_none() {
    assert_eq!(on("w0", Some(&CallTopology::new("w0", "", 1, 0))).other, None);
    assert_eq!(on("w0", None).other, None);
}

/// A copy served the call on its backup, or on a primary holding a write a
/// copy made; a primary that holds none reads no copy.
#[test]
fn a_copy_served_the_call_on_its_backup_or_once_its_write_is_held() {
    let fresh = CallTopology::new("w0", "w1", 3, 0);
    assert!(!on("w0", Some(&fresh)).copy_served);
    assert!(on("w1", Some(&fresh)).copy_served);
    assert!(on("w0", Some(&CallTopology::new("w0", "w1", 3, 1))).copy_served);
    assert!(!on("w0", None).copy_served);
}

/// The merge raises the vector to the component-wise maximum and keeps the
/// container's stamp.
#[test]
fn a_merge_raises_the_vector_and_keeps_the_containers_stamp() {
    let container = Enveloped::new(call(), Some(CallTopology::new("w0", "w1", 6, 1)));
    let mut theirs = CallTopology::new("w0", "w1", 5, 3);
    theirs.stamp_gen = 5;
    let merged = container.merge_exempt(&Enveloped::new(call(), Some(theirs)));
    let t = merged.topology.expect("replicated");
    assert_eq!(t.vector(), (6, 3), "the component-wise maximum");
    assert_eq!(t.stamp(), (6, 1), "the container's stamp");
}
