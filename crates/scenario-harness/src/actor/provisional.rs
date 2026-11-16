//! A held callee's provisional trains and its racing final: one provisional at
//! a time on a chosen early dialog ([`Provisional`]), each early dialog
//! numbering its own `RSeq` space (RFC 3262 §3), and a final sent as soon as
//! §3 allows ([`HeldFinal`]), so PRACKs of several early dialogs may be in
//! flight across one another and across the final.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use tokio::time::Instant;

use super::endpoint::ForkWinner;
use super::held::{answer_held, early_tag, reject_held};
use super::ledger::ObligationKey;
use super::runner::ActorState;
use super::state::Observation;
use crate::{ServerTxn, StepError};

/// One provisional of a [`Disposition::Held`](super::Disposition::Held)
/// callee's INVITE, on the early dialog of `fork` (`None`: the transaction's
/// own tag).
///
/// It is sent reliably when `reliable` and the INVITE offered `100rel`, and
/// always when the INVITE requires it (RFC 3262 §3); a reliable one waits for
/// the PRACK of its dialog's previous reliable provisional and carries the
/// next `RSeq` of that dialog. The first reliable provisional of a dialog
/// carries the session description whatever `sdp` says (RFC 3261 §13.2.1);
/// any other carries it when `sdp`, always the same one. The PRACK that
/// acknowledges it is answered `prack_answer_after` its arrival.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Provisional {
    pub fork: Option<&'static str>,
    pub status: u16,
    pub sdp: bool,
    pub reliable: bool,
    pub prack_answer_after: Duration,
}

/// The final of a [`Disposition::Held`](super::Disposition::Held) callee's
/// INVITE, sent as soon as RFC 3262 §3 allows: a 2xx under `winner` waits only
/// for the PRACK of that dialog's outstanding reliable provisional when it
/// carried a session description; a non-2xx final waits for nothing, and the
/// PRACKs it crosses are still answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeldFinal {
    pub status: u16,
    pub winner: Option<ForkWinner>,
}

/// A held callee's per-early-dialog reliable-provisional bookkeeping.
#[derive(Default)]
pub(super) struct Trains {
    /// The last `RSeq` sent on each early dialog, by local tag.
    rseq: HashMap<String, u32>,
    /// The early dialogs whose outstanding reliable provisional carried a
    /// session description.
    pub(super) offered: HashSet<String>,
    /// How long after its arrival the PRACK of `(tag, RSeq)` is answered.
    pub(super) prack_holds: HashMap<(String, u32), Duration>,
    /// PRACKs taken whose `200` is held, with when it is due and the
    /// obligation that holds the call open for it.
    pub(super) held_200s: Vec<(Instant, ObligationKey, Box<ServerTxn>)>,
    /// How many PRACK 200s were ever held: the next hold's ordinal.
    pub(super) holds_opened: u32,
}

impl Trains {
    /// Forget the early dialogs of a previous initial INVITE.
    pub(super) fn reset(&mut self) {
        self.rseq.clear();
        self.offered.clear();
        self.prack_holds.clear();
    }

    /// Note a reliable provisional sent with `rseq` on `tag` by a path that
    /// numbers its own (`GoalStep::Ring`).
    pub(super) fn note_sent(&mut self, tag: &str, rseq: u32, body: bool) {
        self.rseq.insert(tag.to_string(), rseq);
        if body {
            self.offered.insert(tag.to_string());
        } else {
            self.offered.remove(tag);
        }
    }
}

/// The RSeq values left above a dialog's first one.
const RSEQ_HEADROOM: u64 = 65_536;

/// The first `RSeq` of early dialog `tag` of the INVITE whose Call-ID is
/// `call_id`: in 1..2^31 (RFC 3262 §3), spread by the dialog's identity, with
/// [`RSEQ_HEADROOM`] left so every later RSeq of the dialog stays under 2^31.
fn first_rseq(call_id: &str, tag: &str) -> u32 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for b in call_id.bytes().chain(tag.bytes()) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    1 + (h % ((1u64 << 31) - 1 - RSEQ_HEADROOM)) as u32
}

/// Whether `txn`'s INVITE lets `p` go reliably, and requires it.
fn reliability(txn: &ServerTxn, p: &Provisional) -> bool {
    let req = txn.request();
    let required = req.requires_100rel().unwrap_or(false);
    required || (p.reliable && req.offers_100rel().unwrap_or(false))
}

/// Whether `GoalStep::Provisional` may fire: the INVITE is held, and a
/// reliable provisional's dialog has no reliable one outstanding.
pub(super) fn provisional_ready(st: &ActorState<'_>, p: &Provisional) -> bool {
    let Some(txn) = st.held.as_deref() else { return false };
    !reliability(txn, p)
        || known_tag(st, txn, p.fork).is_none_or(|tag| !st.unpracked.contains_key(&tag))
}

/// The local tag of `fork`'s early dialog, once one exists: an untagged
/// fork's is minted by the transaction's first provisional.
fn known_tag(st: &ActorState<'_>, txn: &ServerTxn, fork: Option<&str>) -> Option<String> {
    (fork.is_some() || !st.early_provisionals.is_empty()).then(|| early_tag(txn, fork))
}

/// `GoalStep::Provisional`: send `p` on its early dialog. The transaction's
/// first provisional names the fork a later non-2xx final goes out on (RFC
/// 3261 §16.7: a forked transaction's non-2xx final is one rung fork's).
pub(super) async fn send_provisional(
    st: &mut ActorState<'_>,
    p: Provisional,
) -> Result<(), StepError> {
    let sdp = st.answer_body();
    let first_on_txn = st.early_provisionals.is_empty();
    let known = st.held.as_deref().and_then(|txn| known_tag(st, txn, p.fork));
    let Some(txn) = st.held.as_deref_mut() else {
        return Err(StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: "Provisional goal with no held initial INVITE".to_string(),
        });
    };
    let reliable = reliability(txn, &p);
    let next = known.as_ref().and_then(|t| st.trains.rseq.get(t)).map(|n| n + 1);
    let rseq = reliable.then(|| {
        let call_id = txn.request().call_id().to_string();
        next.unwrap_or_else(|| first_rseq(&call_id, known.as_deref().unwrap_or_default()))
    });
    let body = p.sdp || (reliable && next.is_none());
    let reason = if p.status == 180 { "Ringing" } else { "Session Progress" };
    let mut r = txn.respond(p.status, reason);
    if let Some(tag) = p.fork {
        r = r.with_to_tag(tag);
    }
    if body {
        r = r.with_sdp(sdp);
    }
    if let Some(rseq) = rseq {
        r = r.reliable(rseq);
    }
    r.try_send().await?;
    let tag = early_tag(txn, p.fork);
    if first_on_txn {
        if let Some(fork) = p.fork {
            txn.adopt_to_tag(fork);
        }
    }
    st.early_provisionals.insert(p.fork.map(str::to_string).unwrap_or_default());
    if let Some(rseq) = rseq {
        st.trains.note_sent(&tag, rseq, body);
        st.trains.prack_holds.insert((tag.clone(), rseq), p.prack_answer_after);
        st.unpracked.entry(tag).or_insert_with(Instant::now);
    }
    st.obs.record(Observation::LegEarly { leg: st.role }, Instant::now());
    Ok(())
}

/// Whether `GoalStep::Final` may fire: the INVITE is held and, for a 2xx, the
/// answering dialog's outstanding reliable provisional carried no session
/// description (RFC 3262 §3). An unrung winner answers a dialog no
/// provisional opened.
pub(super) fn final_ready(st: &ActorState<'_>, f: &HeldFinal) -> bool {
    let Some(txn) = st.held.as_deref() else { return false };
    if f.status >= 300 {
        return true;
    }
    let tag = match f.winner {
        Some(ForkWinner::Unrung) => return true,
        Some(ForkWinner::Tag(tag)) => tag.to_string(),
        None => match known_tag(st, txn, None) {
            Some(tag) => tag,
            None => return true,
        },
    };
    !(st.unpracked.contains_key(&tag) && st.trains.offered.contains(&tag))
}

/// `GoalStep::Final`: the 2xx under the winner, or the non-2xx final.
pub(super) async fn send_final(st: &mut ActorState<'_>, f: HeldFinal) -> Result<(), StepError> {
    if (200..300).contains(&f.status) {
        answer_held(st, f.winner).await
    } else {
        reject_held(st, f.status).await
    }
}
