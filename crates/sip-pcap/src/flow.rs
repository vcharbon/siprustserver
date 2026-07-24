//! Flow model: decoded datagrams → per-Call-ID **legs** (with per-hop
//! observation split) → correlated **call groups** with match evidence.
//!
//! This is the library form of what the `sipflow` bin presents as text:
//! [`build_flows`] runs SIP detection, capture-dup collapsing, parsing with
//! the real `sip-message` parser, leg building and B2BUA leg correlation,
//! and returns the whole model so downstream tooling (extractors, structured
//! emitters) can consume it without scraping human-oriented output.
//!
//! Two structural properties the text output cannot carry are first-class
//! here:
//!
//! - **Hops (vantage separation).** A proxy chain preserves Call-ID, so one
//!   leg mixes the SAME message observed at several capture points. Every
//!   [`FlowMsg`] carries the [`Hop`] (normalized socket pair) it was observed
//!   at, and the leg exposes the hop chain ordered by first observation — a
//!   consumer can select "this leg as seen at hop N".
//! - **Match evidence, not just verdicts.** Correlation is heuristic;
//!   [`MatchEvidence`] records WHY legs were grouped — and WHICH pipeline
//!   strategy fired — so a consumer can display it and let a human confirm
//!   or override the pairing.
//!
//! Correlation is a configurable ordered STRATEGY PIPELINE
//! ([`FlowConfig::strategies`]): per-deployment specificity (which relayed
//! headers, which header params) is config, not code. Strategies run in
//! order; the first that pairs a leg wins.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};

use sip_message::message_helpers::{header_param_value, same_user_identity};
use sip_message::parser::SipParser;
use sip_message::{CustomParser, Method, SipMessage};

use crate::Datagram;

/// Leg index into [`Flows::legs`].
pub type LegId = usize;
/// Hop index into a leg's [`FlowLeg::hops`] chain.
pub type HopId = usize;

/// One correlation strategy of the [`FlowConfig::strategies`] pipeline.
#[derive(Debug, Clone)]
pub enum CorrelateStrategy {
    /// Relayed-token header equality: legs on which any of `headers` carried
    /// the same value are one call.
    HeaderToken { headers: Vec<String> },
    /// Equality of a named `;`-parameter of a named header. The motivating
    /// instance is IMS `P-Charging-Vector` / `icid-value` (the end-to-end
    /// charging id): sibling params mutate across an AS, so whole-header
    /// equality can never work — only the param value is compared.
    HeaderParam { header: String, param: String },
    /// Identity adjacency for token-less pairs: same From/To USER identity
    /// on both INVITEs (`tel:`⇄`sip:` insensitive, host/params ignored), the
    /// pair crossing one shared host at ANY traversed hop (IP-level,
    /// port-insensitive), first activity within the pairing window.
    IdentityAdjacency,
}

/// Correlation configuration for [`build_flows`]: the ordered strategy
/// pipeline. The default is the loadgen relayed-token headers, then the IMS
/// P-Charging-Vector icid, then identity adjacency.
#[derive(Debug, Clone)]
pub struct FlowConfig {
    pub strategies: Vec<CorrelateStrategy>,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            strategies: vec![
                CorrelateStrategy::HeaderToken {
                    headers: vec!["X-Loadgen-Id".to_string(), "X-Api-Call".to_string()],
                },
                CorrelateStrategy::HeaderParam {
                    header: "P-Charging-Vector".to_string(),
                    param: "icid-value".to_string(),
                },
                CorrelateStrategy::IdentityAdjacency,
            ],
        }
    }
}

/// One observation vantage: an unordered UDP socket pair. Requests and their
/// responses travel the same pair in opposite directions, so both directions
/// normalize to the same hop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Hop {
    /// Lower endpoint of the pair (by `(ip, port)` order — NOT a direction).
    pub a: SocketAddr,
    /// Higher endpoint of the pair.
    pub b: SocketAddr,
}

impl Hop {
    fn normalized(x: SocketAddr, y: SocketAddr) -> Self {
        if (x.ip(), x.port()) <= (y.ip(), y.port()) {
            Hop { a: x, b: y }
        } else {
            Hop { a: y, b: x }
        }
    }

    pub fn contains(&self, s: SocketAddr) -> bool {
        self.a == s || self.b == s
    }
}

/// One captured SIP message within a leg.
#[derive(Debug, Clone)]
pub struct FlowMsg {
    /// Capture timestamp, microseconds since the Unix epoch.
    pub ts_us: u64,
    pub src: SocketAddr,
    pub dst: SocketAddr,
    /// Parsed by the real `sip-message` parser.
    pub parsed: SipMessage,
    /// Observation vantage, index into the owning leg's [`FlowLeg::hops`].
    pub hop: HopId,
    /// Same transaction key already seen in this direction on this leg — a
    /// SIP retransmission (capture-stack duplicates are collapsed earlier and
    /// never reach the model, see [`FlowStats::capture_dups`]).
    pub retx: bool,
}

impl FlowMsg {
    /// Exact wire bytes (header order and casing preserved).
    pub fn raw(&self) -> &[u8] {
        match &self.parsed {
            SipMessage::Request(r) => &r.raw,
            SipMessage::Response(r) => &r.raw,
        }
    }
}

/// Initial-INVITE summary of a leg.
#[derive(Debug, Clone)]
pub struct InviteSummary {
    pub ruri: String,
    pub from_uri: String,
    pub to_uri: String,
    pub cseq: u32,
}

/// How a leg was torn down (first teardown request seen).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminator {
    Bye,
    Cancel,
}

impl Terminator {
    pub fn as_str(&self) -> &'static str {
        match self {
            Terminator::Bye => "BYE",
            Terminator::Cancel => "CANCEL",
        }
    }
}

/// All messages sharing one Call-ID, split by observation hop.
#[derive(Debug)]
pub struct FlowLeg {
    pub call_id: String,
    /// Observation vantages, ordered by first observation. A forward-relayed
    /// dialog traverses its capture points in time order, so this follows the
    /// relay chain.
    pub hops: Vec<Hop>,
    /// Messages in capture-time order (across all hops — filter by
    /// [`FlowMsg::hop`] or use [`FlowLeg::msgs_at`] for one vantage).
    pub msgs: Vec<FlowMsg>,
    pub invite: Option<InviteSummary>,
    /// First final (>=200) response to the initial INVITE.
    pub final_status: Option<u16>,
    pub saw_180: bool,
    pub terminated_by: Option<Terminator>,
    /// Correlation token values seen on this leg, parallel to
    /// [`FlowConfig::strategies`] (always the empty set at a non-token
    /// strategy's index).
    pub tokens_by_strategy: Vec<BTreeSet<String>>,
}

impl FlowLeg {
    pub fn t_first(&self) -> u64 {
        self.msgs.first().map(|m| m.ts_us).unwrap_or(0)
    }

    pub fn t_last(&self) -> u64 {
        self.msgs.last().map(|m| m.ts_us).unwrap_or(0)
    }

    /// Union of every token value seen on this leg (all token strategies) —
    /// the display/filter view; per-strategy sets live in
    /// [`FlowLeg::tokens_by_strategy`].
    pub fn tokens(&self) -> BTreeSet<&str> {
        self.tokens_by_strategy.iter().flatten().map(|s| s.as_str()).collect()
    }

    /// (src, dst) of the first INVITE — the leg's direction of establishment.
    pub fn invite_addrs(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.msgs
            .iter()
            .find(|m| matches!(&m.parsed, SipMessage::Request(r) if r.method == Method::Invite))
            .map(|m| (m.src, m.dst))
    }

    /// `(src_ips, dst_ips)` of every INVITE observation on this leg — all
    /// traversed hops, port-insensitive. The B2BUA crossing test needs the
    /// full set: in a multi-hop relayed capture the a-leg's INVITE reaches
    /// the AS host only at its LAST hop, and the b-leg departs the AS from
    /// an ephemeral port.
    pub fn invite_hop_ips(&self) -> (BTreeSet<IpAddr>, BTreeSet<IpAddr>) {
        let mut src = BTreeSet::new();
        let mut dst = BTreeSet::new();
        for m in &self.msgs {
            if matches!(&m.parsed, SipMessage::Request(r) if r.method == Method::Invite) {
                src.insert(m.src.ip());
                dst.insert(m.dst.ip());
            }
        }
        (src, dst)
    }

    /// The leg as seen at one vantage: messages observed at `hop`, in order.
    pub fn msgs_at(&self, hop: HopId) -> impl Iterator<Item = &FlowMsg> {
        self.msgs.iter().filter(move |m| m.hop == hop)
    }
}

/// Why legs were grouped into one call, and which pipeline strategy fired
/// (`strategy` indexes [`FlowConfig::strategies`]). Correlation is heuristic
/// — evidence exists so a consumer can show it and let a human confirm or
/// override.
#[derive(Debug, Clone)]
pub enum MatchEvidence {
    /// A [`CorrelateStrategy::HeaderToken`] header carried the same value on
    /// all `legs`.
    SharedToken { strategy: usize, token: String, legs: Vec<LegId> },
    /// The [`CorrelateStrategy::HeaderParam`] `header`'s `param` carried the
    /// same value on all `legs`.
    SharedHeaderParam { strategy: usize, header: String, param: String, token: String, legs: Vec<LegId> },
    /// [`CorrelateStrategy::IdentityAdjacency`]: same From/To user identity
    /// on both INVITEs, the pair crossing `shared_host` at some traversed
    /// hop, first activity within the pairing window.
    IdentityAdjacency { strategy: usize, legs: [LegId; 2], shared_host: IpAddr, dt_us: u64 },
}

/// Correlated legs of one call.
#[derive(Debug)]
pub struct CallGroup {
    /// Members ordered by first activity.
    pub legs: Vec<LegId>,
    /// What joined the members (empty for a single-leg group).
    pub evidence: Vec<MatchEvidence>,
}

/// Ingest counters (SIP filter, capture dedup, parse). Decode-level counters
/// live in [`crate::DecodeStats`].
#[derive(Debug, Default, Clone)]
pub struct FlowStats {
    /// Messages ingested into the model.
    pub sip_messages: u64,
    /// Identical (src, dst, payload) datagrams within the capture-dup window —
    /// `-i any` seeing one packet on veth AND bridge, not a retransmission.
    pub capture_dups: u64,
    /// SIP-looking datagrams the parser rejected.
    pub parse_failed: u64,
    /// Datagrams that did not look like SIP (RTP/STUN/DNS on captured ports).
    pub non_sip: u64,
}

/// The full flow model of a capture.
#[derive(Debug)]
pub struct Flows {
    pub legs: Vec<FlowLeg>,
    /// Call groups ordered by first activity; every leg is in exactly one.
    pub groups: Vec<CallGroup>,
    pub stats: FlowStats,
}

/// A genuine SIP retransmission is >=500 ms (Timer T1) away; an identical
/// datagram closer than this is the capture stack seeing the packet twice.
const CAPTURE_DUP_WINDOW_US: u64 = 200_000;

/// Max first-activity distance for identity-adjacency pairing.
const FROMTO_PAIR_WINDOW_US: u64 = 5_000_000;

/// Build the flow model: dedup + SIP filter + parse + leg ingest + correlate.
/// Datagrams are processed in capture-time order regardless of input order.
pub fn build_flows(datagrams: &[Datagram], cfg: &FlowConfig) -> Flows {
    let mut order: Vec<usize> = (0..datagrams.len()).collect();
    order.sort_by_key(|&i| datagrams[i].ts_us);

    let mut stats = FlowStats::default();
    let mut dedup: HashMap<u64, u64> = HashMap::new();
    let parser = CustomParser::new();
    let mut legs: Vec<FlowLeg> = Vec::new();
    let mut leg_by_call_id: HashMap<String, usize> = HashMap::new();

    for &i in &order {
        let d = &datagrams[i];
        if !looks_like_sip(&d.payload) {
            stats.non_sip += 1;
            continue;
        }
        let key = hash_datagram(d);
        if let Some(prev) = dedup.get(&key) {
            if d.ts_us.saturating_sub(*prev) < CAPTURE_DUP_WINDOW_US {
                stats.capture_dups += 1;
                continue;
            }
        }
        dedup.insert(key, d.ts_us);

        let msg = match parser.parse(&d.payload) {
            Ok(m) => m,
            Err(_) => {
                stats.parse_failed += 1;
                continue;
            }
        };
        stats.sip_messages += 1;
        let call_id = match &msg {
            SipMessage::Request(r) => r.call_id.clone(),
            SipMessage::Response(r) => r.call_id.clone(),
        };
        let idx = *leg_by_call_id.entry(call_id.clone()).or_insert_with(|| {
            legs.push(FlowLeg {
                call_id,
                hops: Vec::new(),
                msgs: Vec::new(),
                invite: None,
                final_status: None,
                saw_180: false,
                terminated_by: None,
                tokens_by_strategy: vec![BTreeSet::new(); cfg.strategies.len()],
            });
            legs.len() - 1
        });
        ingest(&mut legs[idx], d.ts_us, d.src, d.dst, msg, &cfg.strategies);
    }

    let groups = correlate(&legs, cfg);
    Flows { legs, groups, stats }
}

/// Cheap pre-filter so RTP/STUN/DNS on captured ports never reaches the parser.
fn looks_like_sip(payload: &[u8]) -> bool {
    if payload.len() < 16 {
        return false;
    }
    let head = &payload[..payload.len().min(256)];
    (head.starts_with(b"SIP/2.0 ") || head.windows(9).any(|w| w == b" SIP/2.0\r"))
        && payload[0].is_ascii_uppercase()
}

fn hash_datagram(d: &Datagram) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    d.src.hash(&mut h);
    d.dst.hash(&mut h);
    d.payload.hash(&mut h);
    h.finish()
}

fn ingest(
    leg: &mut FlowLeg,
    ts_us: u64,
    src: SocketAddr,
    dst: SocketAddr,
    msg: SipMessage,
    strategies: &[CorrelateStrategy],
) {
    for (si, strat) in strategies.iter().enumerate() {
        match strat {
            CorrelateStrategy::HeaderToken { headers } => {
                for name in headers {
                    for v in msg.get_header(name) {
                        let v = v.trim();
                        if !v.is_empty() {
                            leg.tokens_by_strategy[si].insert(v.to_string());
                        }
                    }
                }
            }
            CorrelateStrategy::HeaderParam { header, param } => {
                for v in msg.get_header(header) {
                    if let Some(t) = header_param_value(v, param) {
                        if !t.is_empty() {
                            leg.tokens_by_strategy[si].insert(t);
                        }
                    }
                }
            }
            CorrelateStrategy::IdentityAdjacency => {}
        }
    }
    match &msg {
        SipMessage::Request(r) => {
            if r.method == Method::Invite && leg.invite.is_none() {
                leg.invite = Some(InviteSummary {
                    ruri: r.uri.clone(),
                    from_uri: r.from.uri.clone(),
                    to_uri: r.to.uri.clone(),
                    cseq: r.cseq.seq,
                });
            }
            if r.method == Method::Bye && leg.terminated_by.is_none() {
                leg.terminated_by = Some(Terminator::Bye);
            }
            if r.method == Method::Cancel && leg.terminated_by.is_none() {
                leg.terminated_by = Some(Terminator::Cancel);
            }
        }
        SipMessage::Response(r) => {
            if r.cseq.method == Method::Invite {
                if r.status == 180 {
                    leg.saw_180 = true;
                }
                let initial = leg.invite.as_ref().map(|inv| inv.cseq);
                if r.status >= 200
                    && leg.final_status.is_none()
                    && (initial.is_none() || initial == Some(r.cseq.seq))
                {
                    leg.final_status = Some(r.status);
                }
            }
        }
    }
    // Retransmission tag: same direction + same transaction key already seen.
    let retx = leg.msgs.iter().any(|m| {
        m.src == src
            && m.dst == dst
            && match (&m.parsed, &msg) {
                (SipMessage::Request(a), SipMessage::Request(b)) => {
                    a.method == b.method
                        && a.cseq.seq == b.cseq.seq
                        && a.via.first().branch == b.via.first().branch
                }
                (SipMessage::Response(a), SipMessage::Response(b)) => {
                    a.status == b.status
                        && a.cseq == b.cseq
                        && a.via.first().branch == b.via.first().branch
                }
                _ => false,
            }
    });
    let pair = Hop::normalized(src, dst);
    let hop = match leg.hops.iter().position(|h| *h == pair) {
        Some(h) => h,
        None => {
            leg.hops.push(pair);
            leg.hops.len() - 1
        }
    };
    leg.msgs.push(FlowMsg { ts_us, src, dst, parsed: msg, hop, retx });
}

fn find(parent: &mut [usize], mut x: usize) -> usize {
    while parent[x] != x {
        parent[x] = parent[parent[x]];
        x = parent[x];
    }
    x
}

fn union(parent: &mut [usize], a: usize, b: usize) {
    let (ra, rb) = (find(parent, a), find(parent, b));
    if ra != rb {
        parent[ra] = rb;
    }
}

/// Union-find correlation over the configured strategy pipeline, in order.
/// A later strategy never re-pairs legs an earlier one already joined (the
/// first strategy that pairs a leg wins — its evidence is the only one
/// recorded for that join), so evidence lands in pipeline order. Returns the
/// final group list, ordered by first activity, with the evidence that
/// joined each group's members.
fn correlate(legs: &[FlowLeg], cfg: &FlowConfig) -> Vec<CallGroup> {
    let mut parent: Vec<usize> = (0..legs.len()).collect();
    let mut evidence: Vec<MatchEvidence> = Vec::new();

    for (si, strat) in cfg.strategies.iter().enumerate() {
        match strat {
            CorrelateStrategy::HeaderToken { .. } | CorrelateStrategy::HeaderParam { .. } => {
                token_pass(si, strat, legs, &mut parent, &mut evidence);
            }
            CorrelateStrategy::IdentityAdjacency => {
                adjacency_pass(si, legs, &mut parent, &mut evidence);
            }
        }
    }

    // Assemble groups: members by leg order, then order members and groups by
    // first activity; attach each evidence entry to its final group.
    let mut group_of_root: HashMap<usize, usize> = HashMap::new();
    let mut groups: Vec<(usize, CallGroup)> = Vec::new();
    for i in 0..legs.len() {
        let root = find(&mut parent, i);
        let g = *group_of_root.entry(root).or_insert_with(|| {
            groups.push((root, CallGroup { legs: Vec::new(), evidence: Vec::new() }));
            groups.len() - 1
        });
        groups[g].1.legs.push(i);
    }
    for (_, g) in &mut groups {
        g.legs.sort_by_key(|&l| legs[l].t_first());
    }
    groups.sort_by_key(|(_, g)| legs[g.legs[0]].t_first());
    let pos_of_root: HashMap<usize, usize> =
        groups.iter().enumerate().map(|(pos, (root, _))| (*root, pos)).collect();
    let mut groups: Vec<CallGroup> = groups.into_iter().map(|(_, g)| g).collect();
    for ev in evidence {
        let first_leg = match &ev {
            MatchEvidence::SharedToken { legs, .. }
            | MatchEvidence::SharedHeaderParam { legs, .. } => legs[0],
            MatchEvidence::IdentityAdjacency { legs, .. } => legs[0],
        };
        let pos = pos_of_root[&find(&mut parent, first_leg)];
        groups[pos].evidence.push(ev);
    }
    groups
}

/// Token-equality pass (strategy `si`): legs sharing a value in
/// `tokens_by_strategy[si]` are one call. Evidence keeps every leg the value
/// was seen on (first-seen value order — deterministic), but ONLY when the
/// union actually merges legs an earlier strategy had not already joined —
/// the first strategy that pairs a leg wins.
fn token_pass(
    si: usize,
    strat: &CorrelateStrategy,
    legs: &[FlowLeg],
    parent: &mut [usize],
    evidence: &mut Vec<MatchEvidence>,
) {
    let mut token_legs: Vec<(String, Vec<LegId>)> = Vec::new();
    let mut token_idx: HashMap<String, usize> = HashMap::new();
    for (i, leg) in legs.iter().enumerate() {
        for t in &leg.tokens_by_strategy[si] {
            match token_idx.get(t.as_str()) {
                Some(&k) => token_legs[k].1.push(i),
                None => {
                    token_idx.insert(t.clone(), token_legs.len());
                    token_legs.push((t.clone(), vec![i]));
                }
            }
        }
    }
    for (token, ls) in token_legs {
        if ls.len() < 2 {
            continue;
        }
        let roots: BTreeSet<usize> = ls.iter().map(|&l| find(parent, l)).collect();
        if roots.len() < 2 {
            continue; // already one group — an earlier strategy paired these
        }
        for w in ls.windows(2) {
            union(parent, w[0], w[1]);
        }
        evidence.push(match strat {
            CorrelateStrategy::HeaderToken { .. } => {
                MatchEvidence::SharedToken { strategy: si, token, legs: ls }
            }
            CorrelateStrategy::HeaderParam { header, param } => MatchEvidence::SharedHeaderParam {
                strategy: si,
                header: header.clone(),
                param: param.clone(),
                token,
                legs: ls,
            },
            CorrelateStrategy::IdentityAdjacency => unreachable!("not a token strategy"),
        });
    }
}

/// Identity-adjacency pass (strategy `si`), over legs no earlier strategy
/// paired. Candidate pairs: same From/To USER identity on both INVITEs
/// (`tel:`⇄`sip:` insensitive), the INVITEs crossing one shared host at any
/// traversed hop (one leg's INVITE arrives at an IP the other leg's INVITE
/// departs from — IP-level, port-insensitive), first activity within the
/// window. Two legs BOTH carrying values for the same token strategy with
/// none shared are affirmative different-call evidence and never pair (a
/// one-sided/unechoed token does not veto). Joined greedily nearest-first,
/// one partner per leg.
fn adjacency_pass(
    si: usize,
    legs: &[FlowLeg],
    parent: &mut [usize],
    evidence: &mut Vec<MatchEvidence>,
) {
    // A leg is eligible iff it is still alone (its group has one member).
    let mut group_size: HashMap<usize, usize> = HashMap::new();
    for i in 0..legs.len() {
        *group_size.entry(find(parent, i)).or_insert(0) += 1;
    }
    let eligible: Vec<bool> =
        (0..legs.len()).map(|i| group_size[&find(parent, i)] == 1).collect();

    let mut pairs: Vec<(u64, usize, usize, IpAddr)> = Vec::new();
    for i in 0..legs.len() {
        if !eligible[i] || legs[i].invite.is_none() {
            continue;
        }
        let inv_i = legs[i].invite.as_ref().expect("checked above");
        let (src_i, dst_i) = legs[i].invite_hop_ips();
        for j in i + 1..legs.len() {
            if !eligible[j] || legs[j].invite.is_none() {
                continue;
            }
            let inv_j = legs[j].invite.as_ref().expect("checked above");
            if !same_user_identity(&inv_i.from_uri, &inv_j.from_uri)
                || !same_user_identity(&inv_i.to_uri, &inv_j.to_uri)
            {
                continue;
            }
            // Disjoint-token veto: both legs answered the same token
            // strategy and disagreed on every value — different calls.
            let disjoint_tokens = legs[i]
                .tokens_by_strategy
                .iter()
                .zip(&legs[j].tokens_by_strategy)
                .any(|(a, b)| !a.is_empty() && !b.is_empty() && a.is_disjoint(b));
            if disjoint_tokens {
                continue;
            }
            let (src_j, dst_j) = legs[j].invite_hop_ips();
            // The B2BUA crossing at any traversed hop (either orientation);
            // ties broken by lowest IP for determinism.
            let shared_host = match dst_i.intersection(&src_j).next() {
                Some(ip) => *ip,
                None => match dst_j.intersection(&src_i).next() {
                    Some(ip) => *ip,
                    None => continue,
                },
            };
            let dt = legs[j].t_first().abs_diff(legs[i].t_first());
            if dt <= FROMTO_PAIR_WINDOW_US {
                pairs.push((dt, i, j, shared_host));
            }
        }
    }
    pairs.sort_by_key(|(dt, _, _, _)| *dt);
    let mut used: Vec<bool> = vec![false; legs.len()];
    for (dt, i, j, shared_host) in pairs {
        if !used[i] && !used[j] {
            used[i] = true;
            used[j] = true;
            union(parent, i, j);
            evidence.push(MatchEvidence::IdentityAdjacency {
                strategy: si,
                legs: [i, j],
                shared_host,
                dt_us: dt,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dg(ts_us: u64, src: &str, dst: &str, payload: &[u8]) -> Datagram {
        Datagram {
            ts_us,
            src: src.parse().unwrap(),
            dst: dst.parse().unwrap(),
            payload: payload.to_vec(),
        }
    }

    /// Minimal RFC-shaped message; `extra` lines are inserted before the body.
    fn sip_request(method: &str, call_id: &str, cseq: u32, branch: &str, extra: &str) -> Vec<u8> {
        format!(
            "{method} sip:bob@10.0.0.9 SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} {method}\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    fn sip_response(status: u16, reason: &str, call_id: &str, cseq: u32, branch: &str) -> Vec<u8> {
        format!(
            "SIP/2.0 {status} {reason}\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             From: <sip:alice@10.0.0.1>;tag=f1\r\n\
             To: <sip:bob@10.0.0.9>;tag=t1\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: {cseq} INVITE\r\n\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    /// A proxy chain preserves Call-ID: the same INVITE observed at three
    /// vantages plus responses at two of them → 5 ordered (src,dst) pairs but
    /// 3 hops, each message assigned to its vantage.
    #[test]
    fn one_leg_splits_by_observation_hop() {
        let inv = sip_request("INVITE", "hop-split-1", 1, "b1", "");
        let ok = sip_response(200, "OK", "hop-split-1", 1, "b1");
        let datagrams = vec![
            dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
            dg(2_000, "10.0.0.3:5071", "10.0.0.4:5060", &inv),
            dg(3_000, "10.0.0.5:5060", "10.0.0.9:5060", &inv),
            dg(9_000, "10.0.0.9:5060", "10.0.0.5:5060", &ok),
            dg(10_000, "10.0.0.2:5060", "10.0.0.1:5060", &ok),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.legs.len(), 1);
        let leg = &flows.legs[0];
        assert_eq!(leg.hops.len(), 3, "3 vantages from 5 ordered socket pairs");
        assert_eq!(leg.msgs.len(), 5);
        // Hop chain ordered by first observation.
        assert!(leg.hops[0].contains("10.0.0.1:5060".parse().unwrap()));
        assert!(leg.hops[1].contains("10.0.0.3:5071".parse().unwrap()));
        assert!(leg.hops[2].contains("10.0.0.9:5060".parse().unwrap()));
        // Request and its response normalize to the same hop.
        assert_eq!(leg.msgs[0].hop, 0);
        assert_eq!(leg.msgs[4].hop, 0);
        assert_eq!(leg.msgs[2].hop, 2);
        assert_eq!(leg.msgs[3].hop, 2);
        assert_eq!(leg.msgs_at(2).count(), 2);
        // Per-hop selection sees a complete INVITE/200 exchange at hop 0.
        let at0: Vec<_> = leg.msgs_at(0).collect();
        assert!(matches!(&at0[0].parsed, SipMessage::Request(r) if r.method == Method::Invite));
        assert!(matches!(&at0[1].parsed, SipMessage::Response(r) if r.status == 200));
    }

    /// Same (src, dst, payload) within the capture-dup window is the capture
    /// stack (collapsed, counted); past the window it is a real
    /// retransmission (kept, retx-tagged).
    #[test]
    fn capture_dup_collapsed_but_retransmission_kept() {
        let inv = sip_request("INVITE", "dup-1", 1, "b1", "");
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv),
            dg(1_000_050, "10.0.0.1:5060", "10.0.0.2:5060", &inv), // veth+bridge dup
            dg(1_600_000, "10.0.0.1:5060", "10.0.0.2:5060", &inv), // Timer-T1 retx
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.stats.capture_dups, 1);
        assert_eq!(flows.stats.sip_messages, 2);
        let leg = &flows.legs[0];
        assert_eq!(leg.msgs.len(), 2);
        assert!(!leg.msgs[0].retx);
        assert!(leg.msgs[1].retx);
    }

    #[test]
    fn non_sip_and_parse_failures_are_counted_not_ingested() {
        let datagrams = vec![
            dg(1_000, "10.0.0.1:9000", "10.0.0.2:9000", &[0x80; 64]), // RTP-ish
            dg(2_000, "10.0.0.1:5060", "10.0.0.2:5060", b"INVITE sip:x SIP/2.0\r\nGarbage\r\n\r\n"),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.stats.non_sip, 1);
        assert_eq!(flows.stats.parse_failed, 1);
        assert_eq!(flows.stats.sip_messages, 0);
        assert!(flows.legs.is_empty());
    }

    #[test]
    fn shared_token_groups_legs_with_evidence() {
        let a = sip_request("INVITE", "tok-a", 1, "ba", "X-Api-Call: call-77\r\n");
        let b = sip_request("INVITE", "tok-b", 1, "bb", "X-Api-Call: call-77\r\n");
        let lone = sip_request("INVITE", "tok-c", 1, "bc", "X-Api-Call: call-88\r\n");
        let datagrams = vec![
            dg(1_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
            dg(2_000, "10.0.0.2:5062", "10.0.0.9:5060", &b),
            dg(3_000, "10.0.0.7:5060", "10.0.0.8:5060", &lone),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.legs.len(), 3);
        assert_eq!(flows.groups.len(), 2);
        let joined = &flows.groups[0];
        assert_eq!(joined.legs, vec![0, 1]);
        assert_eq!(joined.evidence.len(), 1);
        let MatchEvidence::SharedToken { strategy, token, legs } = &joined.evidence[0] else {
            panic!("expected token evidence, got {:?}", joined.evidence)
        };
        assert_eq!(*strategy, 0);
        assert_eq!(token, "call-77");
        assert_eq!(legs, &vec![0, 1]);
        assert!(flows.groups[1].evidence.is_empty(), "single-leg group carries no evidence");
    }

    /// Token-less captures fall back to identity adjacency; the evidence
    /// names the strategy, the crossing host and the pairing distance.
    #[test]
    fn identity_adjacency_fallback_records_evidence() {
        let a = sip_request("INVITE", "adj-a", 1, "ba", "");
        let b = sip_request("INVITE", "adj-b", 1, "bb", "");
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a), // lands on the B2BUA
            dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b), // departs from it
        ];
        let cfg = FlowConfig::default();
        let flows = build_flows(&datagrams, &cfg);
        assert_eq!(flows.groups.len(), 1);
        let g = &flows.groups[0];
        assert_eq!(g.legs, vec![0, 1]);
        let MatchEvidence::IdentityAdjacency { strategy, legs, shared_host, dt_us } =
            &g.evidence[0]
        else {
            panic!("expected adjacency evidence, got {:?}", g.evidence)
        };
        assert_eq!(*strategy, 2, "identity adjacency is the default pipeline's third strategy");
        assert_eq!(legs, &[0, 1]);
        assert_eq!(*shared_host, "10.0.0.5".parse::<IpAddr>().unwrap());
        assert_eq!(*dt_us, 200_000);

        // Dropping the strategy from the pipeline disables the fallback.
        let token_only = FlowConfig {
            strategies: cfg
                .strategies
                .iter()
                .filter(|s| !matches!(s, CorrelateStrategy::IdentityAdjacency))
                .cloned()
                .collect(),
        };
        let flows = build_flows(&datagrams, &token_only);
        assert_eq!(flows.groups.len(), 2, "fallback disabled → legs stay apart");
    }

    /// INVITE with fully custom R-URI / From / To (the cross-leg correlation
    /// tests need every URI byte-different across the AS).
    fn invite_custom(call_id: &str, branch: &str, ruri: &str, from: &str, to: &str, extra: &str) -> Vec<u8> {
        format!(
            "INVITE {ruri} SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.1:5060;branch=z9hG4bK{branch}\r\n\
             Max-Forwards: 70\r\n\
             From: {from};tag=f-{branch}\r\n\
             To: {to}\r\n\
             Call-ID: {call_id}\r\n\
             CSeq: 1 INVITE\r\n\
             {extra}\
             Content-Length: 0\r\n\r\n"
        )
        .into_bytes()
    }

    const ICID: &str = "8agh3007ghb23oo5h9cc3ns6hpj4l0p96qsq28l5kqn651d8g6-5";

    /// The MOH01 AS-B2B shape: every URI differs across the AS (hosts and
    /// params rewritten, `tel:` vs `sip:` forms) but both INVITEs carry the
    /// same P-Charging-Vector icid-value among mutating sibling params — the
    /// header-param strategy pairs the legs and the evidence names it.
    #[test]
    fn header_param_strategy_pairs_as_b2b_legs_on_icid() {
        let a = invite_custom(
            "moh01-a",
            "ba",
            "sip:+33772589500@orange-mss.fr;user=phone",
            "\"HQ319\" <sip:+33969979518;verstat=TN-Validation-Passed@btip.orange-business.com:5060;user=phone>",
            "<tel:+33772589500>",
            &format!("P-Charging-Vector: icid-value={ICID};icid-generated-at=10.15.252.63\r\n"),
        );
        let b = invite_custom(
            "moh01-b",
            "bb",
            "sip:+33772589500@ims.mnc001.example;user=phone",
            "<sip:+33969979518;verstat=No-TN-Validation@orange-multimedia.fr;user=phone>",
            "<sip:+33772589500@ims.mnc001.example>",
            // Param order variance + mutated siblings: only the icid matches.
            &format!("P-Charging-Vector: orig-ioi=btip.example;icid-value={ICID}\r\n"),
        );
        let datagrams = vec![
            dg(1_000_000, "10.15.193.41:5092", "10.15.193.226:5090", &a),
            dg(1_100_000, "10.15.193.226:54448", "10.20.0.1:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 1, "icid must pair the AS-B2B legs");
        let g = &flows.groups[0];
        assert_eq!(g.legs, vec![0, 1]);
        assert_eq!(g.evidence.len(), 1);
        let MatchEvidence::SharedHeaderParam { strategy, header, param, token, legs } =
            &g.evidence[0]
        else {
            panic!("expected header-param evidence, got {:?}", g.evidence)
        };
        assert_eq!(*strategy, 1, "P-Charging-Vector icid is the default pipeline's second strategy");
        assert_eq!(header, "P-Charging-Vector");
        assert_eq!(param, "icid-value");
        assert_eq!(token, ICID);
        assert_eq!(legs, &vec![0, 1]);
    }

    /// Gap 2: a multi-hop relayed a-leg only reaches the AS host at its LAST
    /// observed hop, and the b-leg departs the AS from an ephemeral port —
    /// the first-observed socket pairs do not cross, so any-hop (IP-level)
    /// crossing is what pairs them.
    #[test]
    fn identity_adjacency_crosses_at_any_traversed_hop() {
        let a = invite_custom(
            "multihop-a",
            "ba",
            "sip:+15550002@as.example",
            "<sip:+15550001@enterprise.example;user=phone>",
            "<tel:+15550002>",
            "",
        );
        let b = invite_custom(
            "multihop-b",
            "bb",
            "sip:+15550002@term.example",
            "<tel:+15550001>",
            "<sip:+15550002@term.example>",
            "",
        );
        let datagrams = vec![
            // a-leg relayed across three hops; the AS (10.0.0.9) appears only
            // as the destination of the LAST one.
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
            dg(1_010_000, "10.0.0.2:5071", "10.0.0.3:5060", &a),
            dg(1_020_000, "10.0.0.3:5092", "10.0.0.9:5090", &a),
            // b-leg departs the AS from an ephemeral port.
            dg(1_200_000, "10.0.0.9:54448", "10.0.0.7:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.legs.len(), 2);
        // The shape genuinely defeats first-socket-pair crossing: neither
        // first-observed INVITE (src, dst) shares an IP the right way round.
        let (sa, da) = flows.legs[0].invite_addrs().unwrap();
        let (sb, db) = flows.legs[1].invite_addrs().unwrap();
        assert_ne!(da.ip(), sb.ip(), "first-pair crossing would already match — shape is wrong");
        assert_ne!(db.ip(), sa.ip(), "first-pair crossing would already match — shape is wrong");

        assert_eq!(flows.groups.len(), 1, "any-hop crossing must pair the legs");
        let g = &flows.groups[0];
        assert_eq!(g.legs, vec![0, 1]);
        let MatchEvidence::IdentityAdjacency { strategy, legs, shared_host, .. } = &g.evidence[0]
        else {
            panic!("expected adjacency evidence, got {:?}", g.evidence)
        };
        assert_eq!(*strategy, 2);
        assert_eq!(legs, &[0, 1]);
        assert_eq!(*shared_host, "10.0.0.9".parse::<IpAddr>().unwrap());
    }

    /// Pipeline order: when both a relayed token and identity adjacency
    /// would pair the same two legs, the evidence records the EARLIER
    /// strategy — and only that one.
    #[test]
    fn earlier_strategy_wins_and_is_the_only_evidence() {
        let a = sip_request("INVITE", "ord-a", 1, "ba", "X-Api-Call: call-42\r\n");
        let b = sip_request("INVITE", "ord-b", 1, "bb", "X-Api-Call: call-42\r\n");
        let datagrams = vec![
            // Adjacency shape too: same From/To users, crossing 10.0.0.5.
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a),
            dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 1);
        let g = &flows.groups[0];
        assert_eq!(g.evidence.len(), 1, "one join → one evidence entry: {:?}", g.evidence);
        let MatchEvidence::SharedToken { strategy, token, .. } = &g.evidence[0] else {
            panic!("expected the earlier (token) strategy, got {:?}", g.evidence)
        };
        assert_eq!(*strategy, 0);
        assert_eq!(token, "call-42");
    }

    /// No over-grouping: different icids, different users, no crossing —
    /// the legs stay apart.
    #[test]
    fn unrelated_legs_are_not_paired() {
        let a = invite_custom(
            "neg-a",
            "ba",
            "sip:+15550002@x.example",
            "<sip:+15550001@x.example>",
            "<sip:+15550002@x.example>",
            "P-Charging-Vector: icid-value=icid-aaa;orig-ioi=x\r\n",
        );
        let b = invite_custom(
            "neg-b",
            "bb",
            "sip:+15559999@y.example",
            "<sip:+15558888@y.example>",
            "<sip:+15559999@y.example>",
            "P-Charging-Vector: icid-value=icid-bbb;orig-ioi=y\r\n",
        );
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
            dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 2, "unrelated legs must not group");
        assert!(flows.groups.iter().all(|g| g.evidence.is_empty()));
    }

    /// Two legs that BOTH carry values for the same token strategy, with no
    /// value shared, are affirmatively different calls — identity adjacency
    /// must not pair them even though users, crossing and window all match
    /// (the token-less shape of this exact scenario pairs, see
    /// `earlier_strategy_wins_and_is_the_only_evidence`).
    #[test]
    fn disjoint_tokens_veto_identity_adjacency() {
        let a = sip_request("INVITE", "veto-a", 1, "ba", "X-Api-Call: call-A\r\n");
        let b = sip_request("INVITE", "veto-b", 1, "bb", "X-Api-Call: call-B\r\n");
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a),
            dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 2, "disagreeing tokens must veto the identity pairing");
        assert!(flows.groups.iter().all(|g| g.evidence.is_empty()));
    }

    /// A one-sided (unechoed) token is NOT a veto: the token-carrying leg
    /// still identity-pairs with a token-less peer.
    #[test]
    fn one_sided_token_does_not_veto_adjacency() {
        let a = sip_request("INVITE", "oneside-a", 1, "ba", "X-Api-Call: call-A\r\n");
        let b = sip_request("INVITE", "oneside-b", 1, "bb", "");
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.5:5060", &a),
            dg(1_200_000, "10.0.0.5:5062", "10.0.0.9:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 1, "an unechoed token must not block the pairing");
        assert!(matches!(
            &flows.groups[0].evidence[0],
            MatchEvidence::IdentityAdjacency { strategy: 2, legs: [0, 1], .. }
        ));
    }

    /// An empty (`icid-value=`) or flag (`icid-value`) param never becomes a
    /// correlation token — the ingest guard drops it.
    #[test]
    fn empty_or_flag_icid_never_correlates() {
        let a = invite_custom(
            "emptyicid-a",
            "ba",
            "sip:+15550002@x.example",
            "<sip:+15550001@x.example>",
            "<sip:+15550002@x.example>",
            "P-Charging-Vector: icid-value=;orig-ioi=x\r\n",
        );
        let b = invite_custom(
            "emptyicid-b",
            "bb",
            "sip:+15559999@y.example",
            "<sip:+15558888@y.example>",
            "<sip:+15559999@y.example>",
            "P-Charging-Vector: icid-value\r\n",
        );
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
            dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert!(flows.legs.iter().all(|l| l.tokens().is_empty()), "empty values are not tokens");
        assert_eq!(flows.groups.len(), 2);
    }

    /// An icid on only one leg pairs nothing — strategy 2 needs the value on
    /// both sides.
    #[test]
    fn icid_on_one_leg_only_does_not_pair() {
        let a = invite_custom(
            "loneicid-a",
            "ba",
            "sip:+15550002@x.example",
            "<sip:+15550001@x.example>",
            "<sip:+15550002@x.example>",
            &format!("P-Charging-Vector: icid-value={ICID};orig-ioi=x\r\n"),
        );
        let b = invite_custom(
            "loneicid-b",
            "bb",
            "sip:+15559999@y.example",
            "<sip:+15558888@y.example>",
            "<sip:+15559999@y.example>",
            "",
        );
        let datagrams = vec![
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
            dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 2);
        assert!(flows.groups.iter().all(|g| g.evidence.is_empty()));
    }

    /// One icid seen on three legs joins all of them into one group, and the
    /// evidence lists every carrier.
    #[test]
    fn three_legs_sharing_one_icid_form_one_group() {
        let pcv = format!("P-Charging-Vector: icid-value={ICID}\r\n");
        let a = sip_request("INVITE", "tri-a", 1, "ba", &pcv);
        let b = sip_request("INVITE", "tri-b", 1, "bb", &pcv);
        let c = sip_request("INVITE", "tri-c", 1, "bc", &pcv);
        let datagrams = vec![
            // Non-crossing addresses: only the icid can group these.
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
            dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
            dg(1_200_000, "10.0.0.5:5060", "10.0.0.6:5060", &c),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 1);
        assert_eq!(flows.groups[0].legs, vec![0, 1, 2]);
        assert_eq!(flows.groups[0].evidence.len(), 1);
        let MatchEvidence::SharedHeaderParam { legs, .. } = &flows.groups[0].evidence[0] else {
            panic!("expected header-param evidence, got {:?}", flows.groups[0].evidence)
        };
        assert_eq!(legs, &vec![0, 1, 2]);
    }

    /// A later strategy can still join a NOT-yet-paired leg into a group an
    /// earlier strategy formed (the partial-overlap union path): the group
    /// carries both strategies' evidence, each naming the legs it joined.
    #[test]
    fn later_strategy_joins_unpaired_leg_into_earlier_group() {
        let icid = format!("P-Charging-Vector: icid-value={ICID}\r\n");
        let a = sip_request("INVITE", "join-a", 1, "ba", "X-Api-Call: call-7\r\n");
        let b = sip_request("INVITE", "join-b", 1, "bb", &format!("X-Api-Call: call-7\r\n{icid}"));
        let c = sip_request("INVITE", "join-c", 1, "bc", &icid);
        let datagrams = vec![
            // Non-crossing addresses: only the tokens can group these.
            dg(1_000_000, "10.0.0.1:5060", "10.0.0.2:5060", &a),
            dg(1_100_000, "10.0.0.3:5060", "10.0.0.4:5060", &b),
            dg(1_200_000, "10.0.0.5:5060", "10.0.0.6:5060", &c),
        ];
        let flows = build_flows(&datagrams, &FlowConfig::default());
        assert_eq!(flows.groups.len(), 1);
        let g = &flows.groups[0];
        assert_eq!(g.legs, vec![0, 1, 2]);
        assert_eq!(g.evidence.len(), 2);
        let MatchEvidence::SharedToken { strategy: 0, legs: tok_legs, .. } = &g.evidence[0] else {
            panic!("expected token evidence first, got {:?}", g.evidence)
        };
        assert_eq!(tok_legs, &vec![0, 1]);
        let MatchEvidence::SharedHeaderParam { strategy: 1, legs: icid_legs, .. } = &g.evidence[1]
        else {
            panic!("expected header-param evidence second, got {:?}", g.evidence)
        };
        assert_eq!(icid_legs, &vec![1, 2]);
    }
}
