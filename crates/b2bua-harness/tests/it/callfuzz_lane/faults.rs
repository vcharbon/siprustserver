//! The network a fuzzed call crosses: an unreliable transport that loses and
//! holds back datagrams (RFC 3261 §18), each party's transaction layer riding
//! it out (§17). The faults stay inside what SIP recovers from: a hold of 2 s
//! at most (under 64·T1), only on a response or an ACK, never on a reliable
//! provisional, whose PRACK is owed only if it arrives before the final
//! (RFC 3262 §4), nor on a 2xx to an INVITE, which a hold could carry past
//! the final to the caller's CANCEL with no loss to explain the order
//! (RFC 3261 §9.2). A loss is recoverable: only a message's first copy may be
//! lost, never a retransmission, so one retransmission recovers each loss,
//! and never a provisional response sent unreliably, which nothing
//! retransmits.
//! Each datagram's fate is drawn from the call's seed and the datagram's place
//! in its sender's flow, never from the order of sends across parties, so a
//! call replays with its faults.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use callfuzz::Rng;
use sip_net_testkit::{Fate, SendFate};

/// The salt of the fault draws: apart from the call's other draws.
const FAULT_SALT: u64 = 0xFA17_5A17_FA17_5A17;

/// The longest a held datagram waits past the transit, in ms.
const LATE_MAX_MS: u64 = 2_000;

/// The percent of datagrams lost and held back.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Faults {
    pub(super) drop: u64,
    pub(super) late: u64,
}

impl Faults {
    pub(super) fn any(&self) -> bool {
        self.drop + self.late > 0
    }
}

/// How many datagrams met each fault in one call.
#[derive(Debug, Default)]
pub(super) struct Tally {
    lost: AtomicU32,
    held: AtomicU32,
}

impl Tally {
    /// The tally as a draw note, empty when no datagram met a fault.
    pub(super) fn note(&self) -> String {
        let [lost, held] = [&self.lost, &self.held].map(|n| n.load(Ordering::Relaxed));
        if lost + held == 0 {
            return String::new();
        }
        format!(" net(lost {lost}, held {held})")
    }
}

/// Whether a hold keeps `raw` within what SIP recovers from: a response that
/// is neither a reliable provisional nor a 2xx to an INVITE, or an ACK.
fn holdable(raw: &[u8]) -> bool {
    match sip_message::sniff::resp_status(raw) {
        Some(200..=299) => sip_message::sniff::cseq_method_token(raw).as_deref() != Some("INVITE"),
        Some(status) => status >= 200 || sip_message::sniff::header_value(raw, "RSeq").is_none(),
        None => sip_message::sniff::req_method(raw).as_deref() == Some("ACK"),
    }
}

/// Whether a retransmission recovers the loss of `raw`: any message but a
/// provisional response sent unreliably (a 1xx without RSeq, the 100
/// included): its sender sends it again only on a retransmission of its
/// request (RFC 3261 §17.2.1), and an INVITE's client sends none past its
/// first provisional (§17.1.1.2).
fn losable(raw: &[u8]) -> bool {
    match sip_message::sniff::resp_status(raw) {
        Some(status) => status >= 200 || sip_message::sniff::header_value(raw, "RSeq").is_some(),
        None => true,
    }
}

/// What makes a datagram a copy of one sent before: a request other than an
/// ACK is retransmitted on its transaction, its top Via branch and method (a
/// CANCEL shares its INVITE's branch, RFC 3261 §9.1); a response, an ACK, or
/// a request naming no branch is retransmitted as the same bytes (§17).
#[derive(Debug, PartialEq, Eq, Hash)]
enum CopyKey {
    Transaction { branch: String, method: String },
    Bytes(Vec<u8>),
}

fn copy_key(raw: &[u8]) -> CopyKey {
    let method = sip_message::sniff::req_method(raw).filter(|m| m != "ACK");
    match (method, sip_message::sniff::via_branch(raw)) {
        (Some(method), Some(branch)) => CopyKey::Transaction { branch, method },
        _ => CopyKey::Bytes(raw.to_vec()),
    }
}

/// A datagram's place in its sender's flow, free of the tokens a party mints
/// at random (tags, branches, Call-ID suffixes): its path, its start line's
/// method or status, its CSeq, and how many datagrams of that same place were
/// sent before it.
fn place_of(
    src: SocketAddr,
    dst: SocketAddr,
    raw: &[u8],
    seen: &mut HashMap<String, u32>,
) -> String {
    let kind = sip_message::sniff::resp_status(raw)
        .map(|s| s.to_string())
        .or_else(|| sip_message::sniff::req_method(raw))
        .unwrap_or_default();
    let cseq = sip_message::sniff::cseq_number(raw).unwrap_or_default();
    let cseq_method = sip_message::sniff::cseq_method_token(raw).unwrap_or_default();
    let place = format!("{src}>{dst} {kind} {cseq} {cseq_method}");
    let nth = seen.entry(place.clone()).or_default();
    *nth += 1;
    format!("{place} #{nth}")
}

/// FNV-1a: a hash stable across runs and builds, unlike the std hasher.
fn stable_hash(text: &str) -> u64 {
    text.bytes()
        .fold(0xcbf2_9ce4_8422_2325, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

/// The fate of each datagram of the call of `seed`, counted in `tally`: the
/// first copy of a [`losable`] message draws its loss, a later copy is never
/// lost.
pub(super) fn fate_of(faults: Faults, seed: u64, tally: Arc<Tally>) -> SendFate {
    let sent = Mutex::new(HashSet::new());
    let places = Mutex::new(HashMap::new());
    Arc::new(move |src, dst, raw| {
        let place = place_of(src, dst, raw, &mut places.lock().expect("places"));
        let mut rng = Rng::new(seed ^ FAULT_SALT ^ stable_hash(&place));
        let first_copy = sent.lock().expect("sent copies").insert(copy_key(raw));
        if first_copy && rng.chance(faults.drop) && losable(raw) {
            tally.lost.fetch_add(1, Ordering::Relaxed);
            return Fate::Lose;
        }
        if holdable(raw) && rng.chance(faults.late) {
            tally.held.fetch_add(1, Ordering::Relaxed);
            return Fate::Late(Duration::from_millis(1 + rng.below(LATE_MAX_MS)));
        }
        Fate::Deliver
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVITE: &[u8] = b"INVITE sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 INVITE\r\n\r\n";
    const RETRY: &[u8] = b"INVITE sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1;rport\r\nCSeq: 1 INVITE\r\n\r\n";
    const CANCEL: &[u8] = b"CANCEL sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 CANCEL\r\n\r\n";
    const TRYING: &[u8] = b"SIP/2.0 100 Trying\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 INVITE\r\n\r\n";
    const RINGING: &[u8] = b"SIP/2.0 180 Ringing\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 INVITE\r\n\r\n";
    const PROGRESS: &[u8] = b"SIP/2.0 183 Session Progress\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 INVITE\r\nRSeq: 1\r\n\r\n";
    const OK: &[u8] =
        b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 INVITE\r\n\r\n";

    /// The fates of `sent`, in order, from 192.0.2.1 to 192.0.2.2 on a fabric
    /// that loses every datagram it may, and its tally's note.
    fn fates_losing_all(sent: &[&[u8]]) -> (Vec<Fate>, String) {
        let tally = Arc::new(Tally::default());
        let fate = fate_of(Faults { drop: 100, late: 0 }, 1, tally.clone());
        let (a, b) = ("192.0.2.1:5060".parse().unwrap(), "192.0.2.2:5060".parse().unwrap());
        (sent.iter().map(|raw| fate(a, b, raw)).collect(), tally.note())
    }

    /// The first copy of each message is lost and every later copy of it
    /// delivered: a request retransmitted on its transaction, a response
    /// sent again as the same bytes, a reliable provisional and a final among
    /// them. A CANCEL on its INVITE's branch is a message of its own.
    #[test]
    fn only_the_first_copy_of_a_message_is_lost() {
        let sent = [INVITE, RETRY, CANCEL, CANCEL, PROGRESS, PROGRESS, OK, OK];
        use Fate::{Deliver, Lose};
        let lost_once = [Lose, Deliver, Lose, Deliver, Lose, Deliver, Lose, Deliver];
        assert_eq!(fates_losing_all(&sent), (lost_once.into(), " net(lost 4, held 0)".into()));
    }

    /// A provisional response sent unreliably, the 100 or another 1xx with
    /// no RSeq, is never lost, its first copy included.
    #[test]
    fn an_unreliable_provisional_is_never_lost() {
        let delivered = vec![Fate::Deliver; 4];
        let sent = [TRYING, RINGING, TRYING, RINGING];
        assert_eq!(fates_losing_all(&sent), (delivered, String::new()));
    }

    /// On a fabric that holds every datagram it may, a 2xx to an INVITE and
    /// a reliable provisional arrive in turn; a 2xx to a CANCEL, a non-2xx
    /// final, an unreliable provisional and an ACK are held.
    #[test]
    fn a_2xx_to_an_invite_is_never_held() {
        const CANCEL_OK: &[u8] = b"SIP/2.0 200 OK\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 CANCEL\r\n\r\n";
        const BUSY: &[u8] = b"SIP/2.0 486 Busy Here\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 INVITE\r\n\r\n";
        const ACK: &[u8] = b"ACK sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-1\r\nCSeq: 1 ACK\r\n\r\n";
        let fate = fate_of(Faults { drop: 0, late: 100 }, 1, Arc::new(Tally::default()));
        let (a, b) = ("192.0.2.1:5060".parse().unwrap(), "192.0.2.2:5060".parse().unwrap());
        let held = |raw: &[u8]| matches!(fate(b, a, raw), Fate::Late(_));
        let sent = [OK, PROGRESS, CANCEL_OK, BUSY, RINGING, ACK];
        let got: Vec<bool> = sent.iter().map(|raw| held(raw)).collect();
        assert_eq!(got, [false, false, true, true, true, true]);
    }

    /// The `n`th request of a flow from 192.0.2.1, on a transaction of its own.
    fn request(n: u32) -> Vec<u8> {
        format!("OPTIONS sip:bob@192.0.2.2 SIP/2.0\r\nVia: SIP/2.0/UDP 192.0.2.1;branch=z9hG4bK-r{n}\r\nCSeq: {n} OPTIONS\r\n\r\n").into_bytes()
    }

    /// The `n`th final of a flow from 192.0.2.2, on a transaction of its own.
    fn final_response(n: u32) -> Vec<u8> {
        format!("SIP/2.0 486 Busy Here\r\nVia: SIP/2.0/UDP 192.0.2.2;branch=z9hG4bK-f{n}\r\nCSeq: {n} INVITE\r\n\r\n").into_bytes()
    }

    /// The fates of two flows, 192.0.2.1's requests and 192.0.2.2's finals,
    /// sent one after the other or interleaved across the two senders, each
    /// flow in its own order: `(requests' fates, finals' fates)`.
    fn fates_of_two_flows(interleaved: bool) -> (Vec<Fate>, Vec<Fate>) {
        let fate = fate_of(Faults { drop: 30, late: 30 }, 7, Arc::new(Tally::default()));
        let (a, b) = ("192.0.2.1:5060".parse().unwrap(), "192.0.2.2:5060".parse().unwrap());
        let (mut requests, mut finals) = (Vec::new(), Vec::new());
        if interleaved {
            for n in 1..=16 {
                requests.push(fate(a, b, &request(n)));
                finals.push(fate(b, a, &final_response(n)));
            }
        } else {
            finals.extend((1..=16).map(|n| fate(b, a, &final_response(n))));
            requests.extend((1..=16).map(|n| fate(a, b, &request(n))));
        }
        (requests, finals)
    }

    /// A datagram's fate follows from its place in its sender's flow alone:
    /// two flows sent interleaved draw the same fates, each loss and each
    /// hold's length, as the same flows sent one after the other.
    #[test]
    fn a_fate_is_the_same_whatever_the_order_of_sends_across_senders() {
        let (requests, finals) = fates_of_two_flows(true);
        assert_eq!((requests.clone(), finals.clone()), fates_of_two_flows(false));
        let all = || requests.iter().chain(&finals);
        assert!(all().any(|f| *f == Fate::Lose), "some datagram lost: {requests:?} {finals:?}");
        assert!(all().any(|f| matches!(f, Fate::Late(_))), "some datagram held: {finals:?}");
    }
}
