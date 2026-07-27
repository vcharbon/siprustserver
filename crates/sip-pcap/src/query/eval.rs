//! Evaluating a predicate tree against the flow model.
//!
//! One rule governs the whole evaluator: **every leaf is evaluated at its own
//! natural level, existentially quantified over the current binding.** A
//! message-level leaf under a group binding asks "does SOME message of this
//! group satisfy it"; the same leaf under a transaction binding asks it of
//! that transaction's messages only. So `{"any_txn": {"all": [{"method":
//! "UPDATE"}, {"final_status": {"ge": 400}}]}}` means the rejection belongs to
//! the same UPDATE, while the two leaves at group level would be satisfied by
//! unrelated messages — which is exactly the distinction a flat filter cannot
//! draw.
//!
//! Quantifier nodes narrow the binding; they never widen it, and applying one
//! at a level it has already reached is the identity.

use sip_message::header::{HeaderName, Reason};
use sip_message::{Method, SipMessage};

use crate::flow::{CallGroup, FlowLeg, Flows, LegId, MatchEvidence};
use crate::txn::{transactions, Txn};

use super::ast::*;

/// Derived views the evaluator needs repeatedly. Transactions are built once
/// per capture, not once per predicate.
pub struct Ctx<'a> {
    pub flows: &'a Flows,
    txns: Vec<Vec<Txn>>,
}

impl<'a> Ctx<'a> {
    pub fn new(flows: &'a Flows) -> Self {
        Ctx { flows, txns: flows.legs.iter().map(transactions).collect() }
    }

    pub fn txns_of_leg(&self, leg: LegId) -> &[Txn] {
        &self.txns[leg]
    }
}

/// What a node is currently evaluated against.
#[derive(Clone, Copy)]
enum Bind<'a> {
    Group(&'a CallGroup),
    Leg(LegId),
    Txn(LegId, &'a Txn),
    /// (leg, index into the leg's messages)
    Msg(LegId, usize),
}

/// The groups of `flows` matching the query's `select` tree, in group order.
pub fn select_groups(flows: &Flows, query: &Query) -> Vec<usize> {
    let ctx = Ctx::new(flows);
    flows
        .groups
        .iter()
        .enumerate()
        .filter(|(_, g)| query.scope.admits(group_t0(flows, g)))
        .filter(|(_, g)| eval(&ctx, Bind::Group(g), &query.select))
        .map(|(i, _)| i)
        .collect()
}

/// First activity across a group's legs.
pub fn group_t0(flows: &Flows, group: &CallGroup) -> u64 {
    group.legs.iter().map(|&l| flows.legs[l].t_first()).min().unwrap_or(0)
}

/// Last activity across a group's legs.
pub fn group_t1(flows: &Flows, group: &CallGroup) -> u64 {
    group.legs.iter().map(|&l| flows.legs[l].t_last()).max().unwrap_or(0)
}

fn eval(ctx: &Ctx, bind: Bind, node: &Node) -> bool {
    match node {
        Node::Always(b) => *b,
        Node::All(ns) => ns.iter().all(|n| eval(ctx, bind, n)),
        Node::Any(ns) => ns.iter().any(|n| eval(ctx, bind, n)),
        Node::Not(n) => !eval(ctx, bind, n),

        Node::AnyLeg(n) => legs_of(bind).iter().any(|&l| eval(ctx, Bind::Leg(l), n)),
        Node::AnyTxn(n) => txn_binds(ctx, bind).iter().any(|&(l, t)| eval(ctx, Bind::Txn(l, t), n)),
        Node::AnyMsg(n) => msgs_of(ctx, bind).iter().any(|&(l, m)| eval(ctx, Bind::Msg(l, m), n)),
        Node::Request(n) => match bind {
            Bind::Txn(l, t) => t.request.is_some_and(|m| eval(ctx, Bind::Msg(l, m), n)),
            Bind::Msg(l, m) => {
                is_request(ctx, l, m) && eval(ctx, Bind::Msg(l, m), n)
            }
            _ => txn_binds(ctx, bind).iter().any(|&(l, t)| {
                t.request.is_some_and(|m| eval(ctx, Bind::Msg(l, m), n))
            }),
        },
        Node::AnyResponse(n) => match bind {
            Bind::Txn(l, t) => t.responses.iter().any(|&m| eval(ctx, Bind::Msg(l, m), n)),
            Bind::Msg(l, m) => !is_request(ctx, l, m) && eval(ctx, Bind::Msg(l, m), n),
            _ => txn_binds(ctx, bind)
                .iter()
                .any(|&(l, t)| t.responses.iter().any(|&m| eval(ctx, Bind::Msg(l, m), n))),
        },

        Node::CountLeg(cmp) => cmp.test(legs_of(bind).len() as u64),
        Node::CountTxn { filter, count } => {
            let n = txn_binds(ctx, bind)
                .iter()
                .filter(|&&(l, t)| {
                    filter.as_ref().is_none_or(|f| eval(ctx, Bind::Txn(l, t), f))
                })
                .count();
            count.test(n as u64)
        }

        // --- group-level leaves ---
        Node::EvidenceKind(kind) => match bind {
            Bind::Group(g) => g.evidence.iter().any(|e| evidence_kind(e) == kind),
            _ => false,
        },
        Node::AsSocket(m) => match bind {
            Bind::Group(g) => g.evidence.iter().any(|e| match e {
                MatchEvidence::DerivedCallId { as_socket, .. } => m.test(&as_socket.to_string()),
                _ => false,
            }),
            _ => false,
        },

        // --- leg-level leaves ---
        Node::CallId(m) => legs_of(bind).iter().any(|&l| m.test(&ctx.flows.legs[l].call_id)),
        Node::Saw180(want) => legs_of(bind).iter().any(|&l| ctx.flows.legs[l].saw_180 == *want),
        Node::TerminatedBy(m) => legs_of(bind).iter().any(|&l| {
            m.test_opt(ctx.flows.legs[l].terminated_by.map(|t| t.as_str()))
        }),
        Node::DurationUs(cmp) => match bind {
            Bind::Group(g) => cmp.test(group_t1(ctx.flows, g).saturating_sub(group_t0(ctx.flows, g))),
            _ => legs_of(bind).iter().any(|&l| {
                let leg = &ctx.flows.legs[l];
                cmp.test(leg.t_last().saturating_sub(leg.t_first()))
            }),
        },

        // --- resolved per binding: the same question at several levels ---
        Node::Ruri(m) => match bind {
            Bind::Msg(l, i) => request_of(ctx, l, i).is_some_and(|r| m.test(&r.request_uri().text())),
            Bind::Txn(l, t) => t
                .request
                .and_then(|i| request_of(ctx, l, i))
                .is_some_and(|r| m.test(&r.request_uri().text())),
            _ => legs_of(bind)
                .iter()
                .any(|&l| {
                    ctx.flows.legs[l].invite.as_ref().is_some_and(|inv| m.test(&inv.ruri.text()))
                }),
        },
        Node::FromUri(m) => uri_leaf(ctx, bind, m, true),
        Node::ToUri(m) => uri_leaf(ctx, bind, m, false),
        Node::FinalStatus(sm) => match bind {
            Bind::Txn(_, t) => sm.test(t.final_status),
            Bind::Msg(..) => false,
            _ => legs_of(bind).iter().any(|&l| {
                // A leg with no INVITE has no initial-INVITE final to speak of,
                // so "none" must not match it and claim a timed-out call.
                ctx.flows.legs[l].invite.is_some() && sm.test(ctx.flows.legs[l].final_status)
            }),
        },
        Node::LatencyUs(cmp) => txn_binds(ctx, bind)
            .iter()
            .any(|(_, t)| t.latency_us.is_some_and(|us| cmp.test(us))),
        Node::TxnKindIs(kind) => txn_binds(ctx, bind).iter().any(|(_, t)| t.kind == *kind),
        Node::MethodIs(name) => match bind {
            Bind::Txn(_, t) => t.method.as_str().eq_ignore_ascii_case(name),
            _ => msgs_of(ctx, bind)
                .iter()
                .any(|&(l, i)| msg_method(ctx, l, i).as_str().eq_ignore_ascii_case(name)),
        },

        // --- message-level leaves ---
        Node::Status(sm) => msgs_of(ctx, bind).iter().any(|&(l, i)| {
            match &ctx.flows.legs[l].msgs[i].parsed {
                SipMessage::Response(r) => sm.test(Some(r.status())),
                SipMessage::Request(_) => false,
            }
        }),
        Node::IsRequest(want) => {
            msgs_of(ctx, bind).iter().any(|&(l, i)| is_request(ctx, l, i) == *want)
        }
        Node::Retx(want) => {
            msgs_of(ctx, bind).iter().any(|&(l, i)| ctx.flows.legs[l].msgs[i].retx == *want)
        }
        Node::Header { name, value } => msgs_of(ctx, bind).iter().any(|&(l, i)| {
            let mut values = ctx.flows.legs[l].msgs[i].parsed.raw(HeaderName::from(name.as_str()));
            match value {
                StrMatch::Absent => values.next().is_none(),
                _ => values.any(|v| value.test(v)),
            }
        }),
        Node::Body(m) => msgs_of(ctx, bind).iter().any(|&(l, i)| {
            let msg = &ctx.flows.legs[l].msgs[i];
            let body = match &msg.parsed {
                SipMessage::Request(r) => &r.body(),
                SipMessage::Response(r) => &r.body(),
            };
            match std::str::from_utf8(body) {
                Ok(s) => m.test(s),
                // A binary body has no text to match; only "absent" can hold.
                Err(_) => matches!(m, StrMatch::Absent) && body.is_empty(),
            }
        }),
        Node::Src(m) => msgs_of(ctx, bind)
            .iter()
            .any(|&(l, i)| m.test(&ctx.flows.legs[l].msgs[i].src.to_string())),
        Node::Dst(m) => msgs_of(ctx, bind)
            .iter()
            .any(|&(l, i)| m.test(&ctx.flows.legs[l].msgs[i].dst.to_string())),
        Node::ReasonCause(cmp) => msgs_of(ctx, bind).iter().any(|&(l, i)| {
            ctx.flows.legs[l].msgs[i]
                .parsed
                .list::<Reason>()
                .unwrap_or_default()
                .iter()
                .filter_map(|r| r.param("cause")?.as_str())
                .filter_map(|c| c.trim().parse::<u64>().ok())
                .any(|c| cmp.test(c))
        }),
    }
}

/// From/To of the bound message, or of the leg's INVITE at a wider binding.
fn uri_leaf(ctx: &Ctx, bind: Bind, m: &StrMatch, from: bool) -> bool {
    match bind {
        Bind::Msg(l, i) => {
            let msg = &ctx.flows.legs[l].msgs[i].parsed;
            let uri = if from { msg.from().uri().clone() } else { msg.to().uri().clone() };
            m.test(&uri.text())
        }
        Bind::Txn(l, t) => t.request.is_some_and(|i| uri_leaf(ctx, Bind::Msg(l, i), m, from)),
        _ => legs_of(bind).iter().any(|&l| {
            ctx.flows.legs[l]
                .invite
                .as_ref()
                .is_some_and(|inv| m.test(&if from { &inv.from_uri } else { &inv.to_uri }.text()))
        }),
    }
}

fn evidence_kind(e: &MatchEvidence) -> &'static str {
    match e {
        MatchEvidence::SharedToken { .. } => "shared_token",
        MatchEvidence::SharedHeaderParam { .. } => "shared_header_param",
        MatchEvidence::DerivedCallId { .. } => "derived_call_id",
        MatchEvidence::IdentityAdjacency { .. } => "identity_adjacency",
    }
}

fn is_request(ctx: &Ctx, leg: LegId, msg: usize) -> bool {
    matches!(&ctx.flows.legs[leg].msgs[msg].parsed, SipMessage::Request(_))
}

/// A message's method: its own for a request, its CSeq's for a response — so
/// `{"method": "UPDATE"}` selects the whole UPDATE exchange, not just the
/// request.
fn msg_method<'a>(ctx: &'a Ctx, leg: LegId, msg: usize) -> &'a Method {
    match &ctx.flows.legs[leg].msgs[msg].parsed {
        SipMessage::Request(r) => r.method(),
        SipMessage::Response(r) => r.cseq().method(),
    }
}

fn request_of<'a>(
    ctx: &'a Ctx,
    leg: LegId,
    msg: usize,
) -> Option<&'a sip_message::SipRequest> {
    match &ctx.flows.legs[leg].msgs[msg].parsed {
        SipMessage::Request(r) => Some(r),
        SipMessage::Response(_) => None,
    }
}

fn legs_of(bind: Bind) -> Vec<LegId> {
    match bind {
        Bind::Group(g) => g.legs.clone(),
        Bind::Leg(l) | Bind::Txn(l, _) | Bind::Msg(l, _) => vec![l],
    }
}

fn txn_binds<'a>(ctx: &'a Ctx, bind: Bind<'a>) -> Vec<(LegId, &'a Txn)> {
    match bind {
        Bind::Txn(l, t) => vec![(l, t)],
        Bind::Msg(l, i) => ctx
            .txns_of_leg(l)
            .iter()
            .filter(|t| t.request == Some(i) || t.responses.contains(&i))
            .map(|t| (l, t))
            .collect(),
        _ => legs_of(bind)
            .into_iter()
            .flat_map(|l| ctx.txns_of_leg(l).iter().map(move |t| (l, t)))
            .collect(),
    }
}

fn msgs_of(ctx: &Ctx, bind: Bind) -> Vec<(LegId, usize)> {
    match bind {
        Bind::Msg(l, i) => vec![(l, i)],
        Bind::Txn(l, t) => {
            t.request.into_iter().chain(t.responses.iter().copied()).map(|i| (l, i)).collect()
        }
        _ => legs_of(bind)
            .into_iter()
            .flat_map(|l| (0..ctx.flows.legs[l].msgs.len()).map(move |i| (l, i)))
            .collect(),
    }
}

/// A leg's transaction view — the join is query-time, so a consumer that
/// wants it outside a query asks for it here.
pub fn leg_transactions(leg: &FlowLeg) -> Vec<Txn> {
    transactions(leg)
}
