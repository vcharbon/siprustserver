//! RFC 3261 §17.1.1.3 — the ACK of a non-2xx INVITE final, owed hop by hop
//! on the INVITE transaction.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, Msg, WireView};

use super::super::sides::Sides;
use super::super::Obligation;

/// How long an open observation must run past a NON-2xx INVITE final before
/// "its ACK never arrived" is charged to the UAS that sent it.
///
/// 33 seconds: §17.2.1 has the rejecting UAS retransmit the final until Timer H
/// (64·T1, 32 s), and an ACK answering any rung of that ladder discharges the
/// transaction — only a capture that ran a second past the give-up point shows
/// none ever came.
pub const REJECT_ACK_WINDOW_US: u64 = 33_000_000;

/// **RFC 3261 §17.1.1.3 — a non-2xx INVITE final is ACKed, at the address
/// its INVITE was sent to.** The ACK of a non-2xx final belongs to the INVITE transaction and
/// reuses its top-Via branch, so it is hop-by-hop: whatever the caller did, a
/// proxy in the path owes THIS UAS an ACK of its own. Without one the reject
/// retransmits to Timer H and its server transaction never completes — the
/// class a per-step expectation cannot see, because every step passed.
///
/// The occasion is ONE non-2xx final an endpoint SENT on a transaction it is
/// the UAS of — it TOOK that transaction's INVITE. Charges the UAC that took
/// the final and owed the ACK; the socket the INVITE reached is the `taker`,
/// the one owed. An ACK on the same branch discharges it only at that socket
/// (§17.1.1.3: the same address, port and transport as the INVITE). The UAC's
/// ACK on the branch to any other socket, such as the one the final came from,
/// leaves it `Violated`, the evidence naming where that ACK went.
///
/// **A retransmitted final re-uses the obligation** (§17.2.1 obliges the
/// repeat, and one transaction owes one ACK), and **an ACK is only a discharge
/// once the obligation exists**: an ACK recorded before the final it answers
/// belongs to some earlier transaction on that branch, not this one.
///
/// **Only a transaction the view saw opened.** A reject on a branch no INVITE in
/// the view carried names no transaction to owe an ACK on, and a 2xx is the
/// other two rules' business, since §13.2.2.4 puts that ACK in the UAC core
/// instead.
pub struct UnackedInviteNon2xxFinal;

impl Obligation for UnackedInviteNon2xxFinal {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let sides = Sides::of(wire.msgs);
        let mut invite: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
        let mut rejects: BTreeMap<(&str, &str, &str), Reject<'_>> = BTreeMap::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            match &msg.kind {
                Kind::Request { method } if method.eq_ignore_ascii_case("INVITE") => {
                    invite.entry((msg.dst.as_str(), msg.call_id.as_str(), branch)).or_insert(mi);
                }
                Kind::Request { method } if method.eq_ignore_ascii_case("ACK") => {
                    for (key, reject) in rejects.iter_mut() {
                        let (uas, call, on) = *key;
                        if call != msg.call_id || on != branch || !reject.owed_by(msg) {
                            continue;
                        }
                        if uas == msg.dst {
                            reject.acked = true;
                        } else {
                            reject.ack_sent_to.get_or_insert(msg.dst.as_str());
                        }
                    }
                }
                Kind::Response { status }
                    if (300..700).contains(status)
                        && msg.cseq_method.eq_ignore_ascii_case("INVITE") =>
                {
                    let key = (sides.uas(msg), msg.call_id.as_str(), branch);
                    let Some(&invite_msg) = invite.get(&key) else { continue };
                    rejects.entry(key).or_insert(Reject {
                        msg: mi,
                        hop: msg.hop,
                        ts_us: msg.at_us,
                        status: *status,
                        invite_msg,
                        taker: msg.dst.as_str(),
                        uac: sides.uac(msg),
                        ack_sent_to: None,
                        cseq: msg.cseq,
                        acked: false,
                    });
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for (key, reject) in &rejects {
            let finding = |decision| Finding {
                rule: RuleId::UnackedInviteNon2xxFinal,
                emitter: reject.taker.to_string(),
                taker: key.0.to_string(),
                cseq: reject.cseq,
                relayed: false,
                anchor: reject.msg,
                decision,
            };
            if reject.acked {
                out.push(finding(Decision::Compliant));
            } else if !wire.obs.absence_decidable(reject.ts_us, REJECT_ACK_WINDOW_US) {
                out.push(finding(Decision::Undecidable(
                    "the observation stopped before the §17.2.1 Timer-H give-up point",
                )));
            } else {
                out.push(finding(Decision::Violated(Evidence::UnackedReject {
                    reject_msg: reject.msg,
                    reject_hop: reject.hop,
                    reject_ts_us: reject.ts_us,
                    status: reject.status,
                    invite_msg: reject.invite_msg,
                    branch: key.2.to_string(),
                    window_us: wire.obs.last_us.saturating_sub(reject.ts_us),
                    ack_sent_to: reject.ack_sent_to.map(str::to_string),
                })));
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// One non-2xx final a UAS sent, and whether its ACK came back.
#[derive(Debug)]
struct Reject<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    /// Index into the view's `msgs` of the INVITE it answers.
    invite_msg: usize,
    /// Where it went — the hop that owes the ACK, and so the one charged.
    taker: &'a str,
    /// The socket the UAC sent its INVITE from.
    uac: &'a str,
    cseq: u32,
    acked: bool,
    /// Where the UAC's first ACK on the branch went, when not to the socket
    /// the INVITE reached.
    ack_sent_to: Option<&'a str>,
}

impl Reject<'_> {
    /// Whether `ack` comes from the UAC that owes this reject its ACK: the
    /// socket its INVITE left from, or the one the reject reached.
    fn owed_by(&self, ack: &Msg) -> bool {
        ack.src == self.uac || ack.src == self.taker
    }
}
