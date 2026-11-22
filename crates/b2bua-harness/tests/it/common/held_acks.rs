//! A fabric hold on the ACKs a UA sends of her re-INVITE's 2xx, which keeps
//! that 2xx awaiting its ACK at the stack while the rest of the call goes on
//! (RFC 3261 §13.3.1.4).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use sip_net_testkit::SendDelay;

/// The hold on the ACKs `ua` sends on another CSeq number than her first
/// ACK's (the ACK of her INVITE's 2xx, never held): the n-th of them is held
/// `holds[n]`, every later one the last of `holds`; with `holds` empty,
/// nothing is held.
pub fn reinvite_acks_held(ua: SocketAddr, holds: &'static [Duration]) -> SendDelay {
    let first_cseq = AtomicU32::new(0);
    let sent = AtomicUsize::new(0);
    Arc::new(move |src, _dst, datagram| {
        if src != ua || !datagram.starts_with(b"ACK ") {
            return None;
        }
        let cseq = sip_message::sniff::cseq_number(datagram)?;
        let first = match first_cseq.compare_exchange(0, cseq, Ordering::Relaxed, Ordering::Relaxed)
        {
            Ok(_) => cseq,
            Err(seen) => seen,
        };
        if cseq == first {
            return None;
        }
        let n = sent.fetch_add(1, Ordering::Relaxed);
        holds.get(n).or(holds.last()).copied()
    })
}
