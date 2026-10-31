//! Wire datagrams placed in the run's recording: each one named by the leg
//! that recorded it and that leg's `seq` ([`MessageRef`]).
//!
//! A datagram is recorded once, by the leg whose endpoint sent or received it,
//! as the bytes that crossed the socket. Placing one is therefore a match on
//! those bytes: the earliest recorded message carrying them that no earlier
//! datagram took. A repeat (a retransmission) carries the same bytes as its
//! original and is recorded after it, so the n-th of identical datagrams
//! lands on the n-th recorded copy.

use std::collections::{BTreeMap, HashMap, VecDeque};

use super::recording::RecordedMessage;
use super::rfc::MessageRef;

/// Each datagram of `wire`, in the order given, as `legs` recorded it; `None`
/// where no leg recorded its bytes, or every recorded copy was already taken.
pub fn locate<'a>(
    wire: impl IntoIterator<Item = &'a [u8]>,
    legs: &BTreeMap<String, Vec<RecordedMessage>>,
) -> Vec<Option<MessageRef>> {
    let mut copies: HashMap<&[u8], VecDeque<MessageRef>> = HashMap::new();
    for (leg, messages) in legs {
        let mut in_order: Vec<&RecordedMessage> = messages.iter().collect();
        in_order.sort_by_key(|m| m.seq);
        for message in in_order {
            copies
                .entry(message.wire())
                .or_default()
                .push_back(MessageRef { leg: leg.clone(), seq: message.seq });
        }
    }
    wire.into_iter().map(|bytes| copies.get_mut(bytes).and_then(VecDeque::pop_front)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::Dir;

    fn message(seq: u64, dir: Dir, at_us: u64, wire: &str) -> RecordedMessage {
        RecordedMessage::new(seq, dir, at_us, None, wire.as_bytes().to_vec())
    }

    const INVITE_A: &str = "INVITE sip:b SIP/2.0\r\nCall-ID: a\r\nCSeq: 1 INVITE\r\n\r\n";
    const TRYING_A: &str = "SIP/2.0 100 Trying\r\nCall-ID: a\r\nCSeq: 1 INVITE\r\n\r\n";
    const INVITE_B: &str = "INVITE sip:c SIP/2.0\r\nCall-ID: b\r\nCSeq: 1 INVITE\r\n\r\n";

    /// Two legs whose datagrams share one instant: the arrival order a reader
    /// would merge them in is not the wire order, and the place is the bytes'.
    #[test]
    fn a_datagram_is_placed_by_its_bytes_whatever_instant_it_shares() {
        let legs = BTreeMap::from([
            (
                "A".to_string(),
                vec![message(1, Dir::Out, 0, INVITE_A), message(2, Dir::In, 0, TRYING_A)],
            ),
            ("B".to_string(), vec![message(1, Dir::In, 0, INVITE_B)]),
        ]);
        // On the wire: A's INVITE, then B's INVITE, then A's 100 — a merge by
        // instant then leg would put the 100 second.
        let placed = locate([INVITE_A.as_bytes(), INVITE_B.as_bytes(), TRYING_A.as_bytes()], &legs);
        assert_eq!(
            placed,
            [
                Some(MessageRef { leg: "A".into(), seq: 1 }),
                Some(MessageRef { leg: "B".into(), seq: 1 }),
                Some(MessageRef { leg: "A".into(), seq: 2 }),
            ]
        );
    }

    #[test]
    fn identical_datagrams_take_the_recorded_copies_in_order() {
        let legs = BTreeMap::from([(
            "A".to_string(),
            vec![message(1, Dir::Out, 0, INVITE_A), message(2, Dir::Out, 500_000, INVITE_A)],
        )]);
        let placed = locate([INVITE_A.as_bytes(), INVITE_A.as_bytes(), INVITE_A.as_bytes()], &legs);
        assert_eq!(placed[0], Some(MessageRef { leg: "A".into(), seq: 1 }));
        assert_eq!(placed[1], Some(MessageRef { leg: "A".into(), seq: 2 }));
        assert_eq!(placed[2], None, "a third copy no leg recorded is placed nowhere");
    }
}
