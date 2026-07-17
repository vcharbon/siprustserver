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
//!   [`MatchEvidence`] records WHY legs were grouped (shared token, or the
//!   From/To + shared-host adjacency fallback) so a consumer can display it
//!   and let a human confirm or override the pairing.

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};

use sip_message::parser::SipParser;
use sip_message::{CustomParser, Method, SipMessage};

use crate::Datagram;

/// Leg index into [`Flows::legs`].
pub type LegId = usize;
/// Hop index into a leg's [`FlowLeg::hops`] chain.
pub type HopId = usize;

/// Correlation configuration for [`build_flows`].
#[derive(Debug, Clone)]
pub struct FlowConfig {
    /// Headers whose (relayed) value ties B2BUA legs together.
    pub correlate_headers: Vec<String>,
    /// Conservative From/To + shared-host adjacency fallback for token-less
    /// captures (see [`MatchEvidence::FromToAdjacency`]).
    pub fromto_fallback: bool,
}

impl Default for FlowConfig {
    fn default() -> Self {
        Self {
            correlate_headers: vec!["X-Loadgen-Id".to_string(), "X-Api-Call".to_string()],
            fromto_fallback: true,
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
    /// Values of the configured correlate headers seen on this leg.
    pub tokens: BTreeSet<String>,
}

impl FlowLeg {
    pub fn t_first(&self) -> u64 {
        self.msgs.first().map(|m| m.ts_us).unwrap_or(0)
    }

    pub fn t_last(&self) -> u64 {
        self.msgs.last().map(|m| m.ts_us).unwrap_or(0)
    }

    /// (src, dst) of the first INVITE — the leg's direction of establishment.
    pub fn invite_addrs(&self) -> Option<(SocketAddr, SocketAddr)> {
        self.msgs
            .iter()
            .find(|m| matches!(&m.parsed, SipMessage::Request(r) if r.method == Method::Invite))
            .map(|m| (m.src, m.dst))
    }

    /// The leg as seen at one vantage: messages observed at `hop`, in order.
    pub fn msgs_at(&self, hop: HopId) -> impl Iterator<Item = &FlowMsg> {
        self.msgs.iter().filter(move |m| m.hop == hop)
    }
}

/// Why legs were grouped into one call. Correlation is heuristic — evidence
/// exists so a consumer can show it and let a human confirm or override.
#[derive(Debug, Clone)]
pub enum MatchEvidence {
    /// A configured correlate header carried the same value on all `legs`.
    SharedToken { token: String, legs: Vec<LegId> },
    /// Token-less fallback: identical From/To URIs on both INVITEs, the pair
    /// crossing one shared host (one leg's INVITE lands on the host the other
    /// leg's INVITE departs from), first activity within the pairing window.
    FromToAdjacency { legs: [LegId; 2], shared_host: IpAddr, dt_us: u64 },
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

/// Max first-activity distance for the From/To adjacency fallback pairing.
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
                tokens: BTreeSet::new(),
            });
            legs.len() - 1
        });
        ingest(&mut legs[idx], d.ts_us, d.src, d.dst, msg, &cfg.correlate_headers);
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
    correlate_headers: &[String],
) {
    for name in correlate_headers {
        for v in msg.get_header(name) {
            let v = v.trim();
            if !v.is_empty() {
                leg.tokens.insert(v.to_string());
            }
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

/// Union-find correlation: tokens first, then the conservative From/To +
/// shared-host adjacency fallback for token-less captures. Returns the final
/// group list, ordered by first activity, with the evidence that joined each
/// group's members.
fn correlate(legs: &[FlowLeg], cfg: &FlowConfig) -> Vec<CallGroup> {
    let mut parent: Vec<usize> = (0..legs.len()).collect();
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

    // Token pass. Legs sharing a token value are one call; the evidence keeps
    // every leg the token was seen on (first-seen token order — deterministic).
    let mut token_legs: Vec<(String, Vec<LegId>)> = Vec::new();
    let mut token_idx: HashMap<String, usize> = HashMap::new();
    for (i, leg) in legs.iter().enumerate() {
        for t in &leg.tokens {
            match token_idx.get(t.as_str()) {
                Some(&k) => {
                    union(&mut parent, i, token_legs[k].1[0]);
                    token_legs[k].1.push(i);
                }
                None => {
                    token_idx.insert(t.clone(), token_legs.len());
                    token_legs.push((t.clone(), vec![i]));
                }
            }
        }
    }

    // Fallback pass. Candidate pairs: same From+To URIs, INVITEs crossing one
    // shared host (a-leg's INVITE destination ip == b-leg's INVITE source ip —
    // the B2BUA), within the window. Joined greedily nearest-first, one
    // partner per leg, and only for legs not already token-correlated.
    let mut adjacency: Vec<MatchEvidence> = Vec::new();
    if cfg.fromto_fallback {
        let mut pairs: Vec<(u64, usize, usize, IpAddr)> = Vec::new();
        for i in 0..legs.len() {
            if !legs[i].tokens.is_empty() {
                continue;
            }
            let (Some(inv_i), Some((si, di))) = (&legs[i].invite, legs[i].invite_addrs()) else {
                continue;
            };
            for j in i + 1..legs.len() {
                if !legs[j].tokens.is_empty() {
                    continue;
                }
                let (Some(inv_j), Some((sj, dj))) = (&legs[j].invite, legs[j].invite_addrs())
                else {
                    continue;
                };
                if inv_i.from_uri != inv_j.from_uri || inv_i.to_uri != inv_j.to_uri {
                    continue;
                }
                // The B2BUA crossing: one leg's INVITE lands on the host the
                // other leg's INVITE departs from (either orientation).
                let shared_host = if di.ip() == sj.ip() {
                    di.ip()
                } else if dj.ip() == si.ip() {
                    dj.ip()
                } else {
                    continue;
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
                union(&mut parent, i, j);
                adjacency.push(MatchEvidence::FromToAdjacency {
                    legs: [i, j],
                    shared_host,
                    dt_us: dt,
                });
            }
        }
    }

    // Assemble groups: members by leg order, then order members and groups by
    // first activity.
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
    for (token, ls) in token_legs {
        if ls.len() >= 2 {
            let pos = pos_of_root[&find(&mut parent, ls[0])];
            groups[pos].evidence.push(MatchEvidence::SharedToken { token, legs: ls });
        }
    }
    for ev in adjacency {
        let MatchEvidence::FromToAdjacency { legs: [i, _], .. } = &ev else { unreachable!() };
        let pos = pos_of_root[&find(&mut parent, *i)];
        groups[pos].evidence.push(ev);
    }
    groups
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
        let MatchEvidence::SharedToken { token, legs } = &joined.evidence[0] else {
            panic!("expected token evidence, got {:?}", joined.evidence)
        };
        assert_eq!(token, "call-77");
        assert_eq!(legs, &vec![0, 1]);
        assert!(flows.groups[1].evidence.is_empty(), "single-leg group carries no evidence");
    }

    /// Token-less captures fall back to From/To + shared-host adjacency; the
    /// evidence names the crossing host and the pairing distance.
    #[test]
    fn fromto_adjacency_fallback_records_evidence() {
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
        let MatchEvidence::FromToAdjacency { legs, shared_host, dt_us } = &g.evidence[0] else {
            panic!("expected adjacency evidence, got {:?}", g.evidence)
        };
        assert_eq!(legs, &[0, 1]);
        assert_eq!(*shared_host, "10.0.0.5".parse::<IpAddr>().unwrap());
        assert_eq!(*dt_us, 200_000);

        let flows = build_flows(&datagrams, &FlowConfig { fromto_fallback: false, ..cfg });
        assert_eq!(flows.groups.len(), 2, "fallback disabled → legs stay apart");
    }
}
