//! RFC 3261 §16.3 — a proxy with no resolvable target answers 404.

use std::collections::{BTreeMap, BTreeSet};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, WireView};

use super::super::correlation::Parties;
use super::super::sides::Sides;
use super::super::Obligation;

/// **RFC 3261 §16.3 — a proxy with no resolvable target answers 404.** When
/// target resolution yields nothing, "Not Found" is the answer that names the
/// outcome; any other error final tells the UAC the request failed for a reason
/// that did not happen, and a retry or a fallback is chosen on that lie.
///
/// The occasion is ONE request the endpoint took, never forwarded, and answered
/// with a 4xx/5xx/6xx final of its own. Charges that endpoint; a 404 discharges
/// it. Nothing else is an occasion: a forwarded request had a target, a
/// non-error final is not a resolution failure, and a request answered nothing
/// at all states no outcome to judge.
///
/// **The endpoint is a user agent on its call ([`Parties`])**: the socket a
/// request reached, the socket that answered it, the one it forwarded from and
/// the one it took the downstream final at are one proxy wherever its
/// transactions tie them.
///
/// **"Forwarded" is correlated by Call-ID + CSeq, never by branch.** A §16.6
/// proxy mints a FRESH branch on the leg it forwards, so branch equality
/// systematically misses the forward and would charge every relaying proxy.
///
/// **A request the strict reading refuses is no occasion** ([`Msg::refused`](crate::wire::Msg::refused)):
/// its 400 says what went wrong, and no target was looked for.
///
/// **A relayed rejection is not a no-target outcome**: where the endpoint TOOK
/// that same final status on the same Call-ID + CSeq, it did resolve a target
/// and the target said no — even when the forwarded leg itself rode a different
/// vantage.
pub struct NoTarget404;

impl Obligation for NoTarget404 {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        let sides = Sides::of(wire.msgs);
        let parties = Parties::of(wire.msgs);
        let mut forwarded: BTreeSet<(String, &str, u32, String)> = BTreeSet::new();
        let mut took_final: BTreeSet<(String, &str, u32, String, u16)> = BTreeSet::new();
        let mut taken: BTreeMap<(&str, &str, &str), usize> = BTreeMap::new();
        let mut answered: BTreeMap<(&str, &str, &str), u16> = BTreeMap::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let call = msg.call_id.as_str();
            let txn_method = msg.cseq_method.to_ascii_uppercase();
            let branch = msg.via_branch.as_deref().filter(|b| !b.is_empty());
            match &msg.kind {
                Kind::Request { .. } => {
                    forwarded.insert((parties.sender(msg).0, call, msg.cseq, txn_method));
                    if let Some(branch) = branch.filter(|_| !msg.refused) {
                        taken.entry((msg.dst.as_str(), call, branch)).or_insert(mi);
                    }
                }
                Kind::Response { status } if *status >= 200 => {
                    took_final.insert((parties.taker(msg).0, call, msg.cseq, txn_method, *status));
                    if let Some(branch) = branch {
                        answered.entry((sides.uas(msg), call, branch)).or_insert(*status);
                    }
                }
                Kind::Response { .. } => {}
            }
        }

        let mut out = Vec::new();
        for ((endpoint, call, branch), mi) in &taken {
            let msg = &wire.msgs[*mi];
            let txn_method = msg.cseq_method.to_ascii_uppercase();
            let proxy = parties.taker(msg).0;
            if forwarded.contains(&(proxy.clone(), call, msg.cseq, txn_method.clone())) {
                continue;
            }
            let Some(&status) = answered.get(&(endpoint, call, branch)) else { continue };
            if !(400..700).contains(&status) {
                continue;
            }
            if took_final.contains(&(proxy, call, msg.cseq, txn_method, status)) {
                continue; // the downstream's rejection, relayed
            }
            let Kind::Request { method } = &msg.kind else { continue };
            out.push(Finding {
                rule: RuleId::NoTarget404,
                emitter: endpoint.to_string(),
                taker: msg.src.to_string(),
                cseq: msg.cseq,
                relayed: false,
                anchor: *mi,
                decision: if status == 404 {
                    Decision::Compliant
                } else {
                    Decision::Violated(Evidence::NoTargetFinal {
                        no_target_msg: *mi,
                        no_target_hop: msg.hop,
                        no_target_ts_us: msg.at_us,
                        method: method.to_string(),
                        branch: branch.to_string(),
                        status,
                    })
                },
            });
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}
