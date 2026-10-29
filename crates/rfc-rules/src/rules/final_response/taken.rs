//! RFC 3261 §17.2.1 / §13.3.1.1, RFC 6026 §7.1 — an INVITE server transaction
//! as its TAKER saw it answered, whichever server-side sender answered it.
//!
//! A transaction can be answered by more than one sender: a server that
//! hands its transactions to another mid-call puts both on the wire under the
//! one branch. The taker sees one transaction. Once a final reached it, no
//! provisional and no final of another kind reaches it on that transaction:
//! after a non-2xx only that final's repeats, after a 2xx only 2xx (RFC 6026
//! §7.1 has a 2xx's repeats and its forked siblings' pass in Accepted).
//!
//! The occasion is a response from a sender OTHER than the one that sent the
//! taker's first final: what one sender does after its own final is
//! [`SingleFinalPerServerTxn`](super::SingleFinalPerServerTxn) and
//! [`No1xxAfterFinal`](super::No1xxAfterFinal)'s. A 100 Trying presents no
//! early dialog and is never one. The transaction is scoped by Call-ID,
//! branch and CSeq, and ends at its timers, as the per-sender siblings'.
//! Charges the late response's sender.

use std::collections::BTreeMap;

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::WireView;

use super::super::Obligation;
use super::lifetime_us;

/// **The taker's INVITE transaction ends at its final.** See the module doc.
pub struct TakenFinalEndsInviteTxn;

/// The first final a taker took on one transaction.
struct Taken<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    status: u16,
    sender: &'a str,
}

impl Obligation for TakenFinalEndsInviteTxn {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let mut taken: BTreeMap<(&str, &str, &str, u32), Taken<'_>> = BTreeMap::new();
        let mut out = Vec::new();
        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(status) = msg.status().filter(|s| *s > 100) else { continue };
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                continue;
            }
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            let key = (msg.dst.as_str(), msg.call_id.as_str(), branch, msg.cseq);
            let Some(first) = taken.get(&key) else {
                if status >= 200 {
                    let first = Taken {
                        msg: mi,
                        hop: msg.hop,
                        ts_us: msg.at_us,
                        status,
                        sender: msg.src.as_str(),
                    };
                    taken.insert(key, first);
                }
                continue;
            };
            let ended = first.ts_us.saturating_add(lifetime_us("INVITE", first.status));
            if first.sender == msg.src || msg.at_us > ended {
                continue;
            }
            let decision = match (status, first.status) {
                (s, f) if s >= 200 && (s == f || (s < 300 && f < 300)) => Decision::Compliant,
                (s, _) if s < 200 => Decision::Violated(Evidence::LateProvisional {
                    late_provisional_msg: mi,
                    late_provisional_hop: msg.hop,
                    late_provisional_ts_us: msg.at_us,
                    status: s,
                    to_tag: msg.to_tag.clone().unwrap_or_default(),
                    completed_by_msg: first.msg,
                    completed_by_status: first.status,
                    gap_us: msg.at_us.saturating_sub(first.ts_us),
                    branch: branch.to_string(),
                }),
                (s, _) => Decision::Violated(Evidence::MultipleFinals {
                    first_msg: first.msg,
                    first_hop: first.hop,
                    first_ts_us: first.ts_us,
                    first_status: first.status,
                    second_msg: mi,
                    second_hop: msg.hop,
                    second_ts_us: msg.at_us,
                    second_status: s,
                    divergent: vec![s],
                    gap_us: msg.at_us.saturating_sub(first.ts_us),
                    method: "INVITE".to_string(),
                    branch: branch.to_string(),
                }),
            };
            out.push(Finding {
                rule: RuleId::TakenFinalEndsInviteTxn,
                emitter: msg.src.to_string(),
                taker: msg.dst.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: mi,
                decision,
            });
        }
        out
    }
}
