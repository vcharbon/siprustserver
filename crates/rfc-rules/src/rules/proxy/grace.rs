//! RFC 3261 §16.7 — a hop that holds an INVITE past the grace says it is
//! trying.

use std::collections::{BTreeMap, BTreeSet};

use crate::verdict::{Decision, Evidence, Finding, RuleId};
use crate::wire::{Kind, WireView};

use super::super::sides::Sides;
use super::super::Obligation;

/// The §16.7 grace: how long a hop may hold an INVITE before it owes the UAC a
/// 100 Trying, microseconds.
///
/// 200 ms, as §16.7 states it. The rules read microseconds while the live
/// vantage stamps milliseconds and bumps a tied stamp by one microsecond to keep
/// its view strictly ordered, so a Δ this rule computes can exceed the
/// millisecond Δ by at most the number of messages sharing one millisecond — a
/// bound that only ever matters within microseconds of exactly 200 ms.
pub const PROXY_100_GRACE_US: u64 = 200_000;

/// **RFC 3261 §16.7 — a hop that cannot answer promptly says it is trying.**
/// A stateful proxy relaying an INVITE emits its own 100 Trying toward the UAC,
/// so the caller's Timer A stops retransmitting into a path that has the request
/// in hand. §16.7 only obliges it where the final cannot be produced promptly:
/// a hop that relays a 486 in two milliseconds owes nobody a Trying.
///
/// The occasion is ONE INVITE the endpoint TOOK on a transaction. Charges that
/// endpoint; a 100 on the transaction discharges it, and so does any final
/// within [`PROXY_100_GRACE_US`] of the INVITE.
///
/// **Proxy-scoped by consumer policy.** §16.7 binds proxies — a UAS answering
/// an INVITE is governed by §8.2.6. An INVITE the strict reading refuses
/// ([`crate::Msg::refused`]) is no occasion: the hop answers it 400 or drops it.
pub struct Proxy100WithinGrace;

impl Obligation for Proxy100WithinGrace {
    fn eval(&self, wire: &WireView<'_>) -> Vec<Finding> {
        // Per (taker, call, branch): the INVITE it took, whether it said it was
        // trying, and its earliest final on the transaction — a response filed
        // under the socket its INVITE reached, whichever socket sent it.
        let sides = Sides::of(wire.msgs);
        let mut taken: BTreeMap<(&str, &str, &str), Taken<'_>> = BTreeMap::new();
        let mut said_trying: BTreeSet<(&str, &str, &str)> = BTreeSet::new();
        let mut first_final: BTreeMap<(&str, &str, &str), u64> = BTreeMap::new();

        for (mi, msg) in wire.msgs.iter().enumerate() {
            let Some(branch) = msg.via_branch.as_deref().filter(|b| !b.is_empty()) else {
                continue;
            };
            if !msg.cseq_method.eq_ignore_ascii_case("INVITE") {
                continue;
            }
            match &msg.kind {
                Kind::Request { method }
                    if method.eq_ignore_ascii_case("INVITE") && !msg.refused =>
                {
                    taken.entry((msg.dst.as_str(), msg.call_id.as_str(), branch)).or_insert(
                        Taken {
                            msg: mi,
                            hop: msg.hop,
                            ts_us: msg.at_us,
                            cseq: msg.cseq,
                            sender: msg.src.as_str(),
                        },
                    );
                }
                Kind::Response { status } => {
                    let key = (sides.uas(msg), msg.call_id.as_str(), branch);
                    if *status == 100 {
                        said_trying.insert(key);
                    } else if *status >= 200 {
                        let at = first_final.entry(key).or_insert(msg.at_us);
                        *at = (*at).min(msg.at_us);
                    }
                }
                _ => {}
            }
        }

        let mut out = Vec::new();
        for (key, invite) in &taken {
            let finding = |decision| Finding {
                rule: RuleId::Proxy100WithinGrace,
                emitter: key.0.to_string(),
                taker: invite.sender.to_string(),
                cseq: invite.cseq,
                relayed: false,
                anchor: invite.msg,
                decision,
            };
            if said_trying.contains(key) {
                out.push(finding(Decision::Compliant));
                continue;
            }
            let after = first_final.get(key).map(|at| at.saturating_sub(invite.ts_us));
            match after {
                // §16.7 wants the 100 only where the answer is slow.
                Some(delta) if delta <= PROXY_100_GRACE_US => {
                    out.push(finding(Decision::Compliant))
                }
                // Nothing at all went out, and the observation may simply have
                // stopped before it could.
                None if !wire.obs.absence_decidable(invite.ts_us, PROXY_100_GRACE_US) => {
                    out.push(finding(Decision::Undecidable(
                        "the observation stopped inside the grace — truncation, not silence",
                    )))
                }
                _ => out.push(finding(Decision::Violated(Evidence::TryingNotSentInGrace {
                    trying_owed_msg: invite.msg,
                    trying_owed_hop: invite.hop,
                    trying_owed_ts_us: invite.ts_us,
                    branch: key.2.to_string(),
                    first_final_after_us: after,
                    grace_us: PROXY_100_GRACE_US,
                }))),
            }
        }
        out.sort_by_key(|f| f.anchor);
        out
    }
}

/// The INVITE one endpoint took on one transaction.
#[derive(Debug)]
struct Taken<'a> {
    msg: usize,
    hop: usize,
    ts_us: u64,
    cseq: u32,
    /// The party that sent it — the one owed the 100.
    sender: &'a str,
}
