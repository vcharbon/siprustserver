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
//! headers, which header params) is config, not code.
//!
//! GROUPING is first-wins — a later strategy never re-unions legs an earlier
//! one already joined. EVIDENCE is not: every strategy that independently
//! attests a pairing records it. The two differ because evidence is read, not
//! just displayed — resolving which socket of an application-server loopback
//! is the AS needs the derivation attestation even when a charging-id header
//! already merged the same legs. `IdentityAdjacency` is the exception and
//! stays gated to still-unpaired legs: it is the weakest fallback, and
//! attesting a pairing a specific strategy already explained would assert an
//! identity match on no better ground than "nothing else fired".

use std::collections::{BTreeSet, HashMap};
use std::net::{IpAddr, SocketAddr};

use sip_message::header::{HeaderName, Params, Uri};
use sip_message::parser::SipParser;
use sip_message::{CustomParser, Method, SipMessage};

use crate::align::{align_probes, rides_no_timer};
use crate::Datagram;

mod stats;
pub use stats::{FlowStats, HeaderFailure};

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
    /// Call-ID derivation: an application server that loops a call back to
    /// the peer it arrived from re-emits it under `<prefix><original>` (the
    /// `1-` / `term1-` relay convention). The socket the derived INVITE
    /// departs on IS the AS; requiring it to be a socket pair the base leg
    /// also traversed is what separates a loopback from a coincidence.
    DerivedCallId {
        /// Max |first-activity| distance between the two legs.
        window_us: u64,
        /// Require the derived INVITE to depart on a hop of the base leg.
        require_shared_hop: bool,
        /// Shortest base Call-ID that may carry a derivation — a short id is
        /// a suffix of an unrelated one too easily.
        min_base_len: usize,
    },
    /// Identity adjacency for token-less pairs: same From/To USER identity
    /// on both INVITEs (`tel:`⇄`sip:` insensitive, host/params ignored), the
    /// pair crossing one shared host at ANY traversed hop (IP-level,
    /// port-insensitive), first activity within the pairing window.
    IdentityAdjacency { window_us: u64 },
}

/// Correlation configuration for [`build_flows`]: the ordered strategy
/// pipeline. The default runs the most specific evidence first — loadgen
/// relayed-token headers, the IMS P-Charging-Vector icid, Call-ID derivation
/// — and falls back to identity adjacency.
#[derive(Debug, Clone)]
pub struct FlowConfig {
    pub strategies: Vec<CorrelateStrategy>,
    /// Identical `(src, dst, payload)` closer together than this is the
    /// capture stack seeing one packet twice, not a retransmission — how far
    /// back to look is a property of the capture, so it is configuration.
    /// Also the tolerance of the probe alignment ([`crate::align`]) that puts
    /// a merged capture's probes on one clock first.
    pub dedup_window_us: u64,
}

/// Default pairing window for the strategies that bound one: two legs of one
/// call start within seconds of each other, never minutes.
pub const DEFAULT_PAIR_WINDOW_US: u64 = 5_000_000;

/// Default capture-dedup window: a genuine SIP retransmission is >=500 ms
/// (Timer T1) away, so anything closer is the capture stack, not the wire.
pub const DEFAULT_DEDUP_WINDOW_US: u64 = 200_000;

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
                CorrelateStrategy::DerivedCallId {
                    window_us: DEFAULT_PAIR_WINDOW_US,
                    require_shared_hop: true,
                    min_base_len: 8,
                },
                CorrelateStrategy::IdentityAdjacency { window_us: DEFAULT_PAIR_WINDOW_US },
            ],
            dedup_window_us: DEFAULT_DEDUP_WINDOW_US,
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
    /// The same DATAGRAM already seen in this direction on this leg, inside the
    /// transaction envelope — a SIP retransmission (capture-stack duplicates
    /// are collapsed earlier and never reach the model, see
    /// [`FlowStats::capture_dups`]). The MODEL's own view: the emitted
    /// document's `retx` is decided by `crate::callfacts::mark_repeats` on
    /// the same two bounds.
    pub retx: bool,
    /// Which probe wrote this copy ([`crate::Datagram::probe`]). Carried, not
    /// derived: two copies of one packet from two probes are identical in every
    /// other field, so this is the ONLY thing that tells them apart.
    pub probe: u32,
}

impl FlowMsg {
    /// Exact wire bytes (header order and casing preserved).
    pub fn raw(&self) -> &[u8] {
        match &self.parsed {
            SipMessage::Request(r) => r.image(),
            SipMessage::Response(r) => r.image(),
        }
    }
}

/// Initial-INVITE summary of a leg. The addresses are the parsed values, so a
/// consumer asks them for a user identity instead of peeling their text.
#[derive(Debug, Clone)]
pub struct InviteSummary {
    pub ruri: Uri,
    pub from_uri: Uri,
    pub to_uri: Uri,
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
        self.invite_observations().next()
    }

    /// Every INVITE observation as `(src, dst)`, in capture order — one entry
    /// per traversed hop, so a relayed INVITE appears once per vantage.
    pub fn invite_observations(&self) -> impl Iterator<Item = (SocketAddr, SocketAddr)> + '_ {
        self.msgs
            .iter()
            .filter(|m| matches!(&m.parsed, SipMessage::Request(r) if r.method() == Method::Invite))
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
            if matches!(&m.parsed, SipMessage::Request(r) if r.method() == Method::Invite) {
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
    SharedHeaderParam {
        strategy: usize,
        header: String,
        param: String,
        token: String,
        legs: Vec<LegId>,
    },
    /// [`CorrelateStrategy::DerivedCallId`]: `legs[1]`'s Call-ID is `prefix`
    /// ++ `legs[0]`'s. `as_socket` emitted the derived INVITE, `peer_socket`
    /// received it — the application server and the peer it looped back to.
    DerivedCallId {
        strategy: usize,
        legs: [LegId; 2],
        prefix: String,
        as_socket: SocketAddr,
        peer_socket: SocketAddr,
        /// The derived INVITE departed on a socket pair the base leg also
        /// traversed — the loopback proof, absent when not required.
        shared_hop: bool,
        dt_us: u64,
    },
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

/// The full flow model of a capture.
#[derive(Debug)]
pub struct Flows {
    pub legs: Vec<FlowLeg>,
    /// Call groups ordered by first activity; every leg is in exactly one.
    pub groups: Vec<CallGroup>,
    pub stats: FlowStats,
}

/// Build the flow model: probe alignment + dedup + SIP filter + parse + leg
/// ingest + correlate. Datagrams are processed in capture-time order
/// regardless of input order — the order of the ALIGNED timestamps, which
/// is also what every [`FlowMsg::ts_us`] carries.
pub fn build_flows(datagrams: &[Datagram], cfg: &FlowConfig) -> Flows {
    let parser = CustomParser::new();
    let keys: Vec<Option<u64>> =
        datagrams.iter().map(|d| looks_like_sip(&d.payload).then(|| hash_datagram(d))).collect();
    let (ts, aligned_probes) = align_probes(datagrams, &keys, cfg.dedup_window_us, |i| {
        parser.parse(&datagrams[i].payload).is_ok_and(|m| rides_no_timer(&m))
    });
    let mut order: Vec<usize> = (0..datagrams.len()).collect();
    order.sort_by_key(|&i| ts[i]);

    let mut stats = FlowStats { aligned_probes, ..FlowStats::default() };
    let mut dedup: HashMap<u64, u64> = HashMap::new();
    let mut legs: Vec<FlowLeg> = Vec::new();
    let mut leg_by_call_id: HashMap<String, usize> = HashMap::new();

    for &i in &order {
        let d = &datagrams[i];
        let Some(key) = keys[i] else {
            stats.non_sip += 1;
            continue;
        };
        if let Some(prev) = dedup.get(&key) {
            if ts[i].saturating_sub(*prev) < cfg.dedup_window_us {
                stats.capture_dups += 1;
                continue;
            }
        }
        dedup.insert(key, ts[i]);

        let msg = match parser.parse(&d.payload) {
            Ok(m) => m,
            Err(_) => {
                stats.parse_failed += 1;
                stats.parse_failed_at.push(i);
                continue;
            }
        };
        stats.sip_messages += 1;
        let call_id = match &msg {
            SipMessage::Request(r) => r.call_id().clone(),
            SipMessage::Response(r) => r.call_id().clone(),
        };
        let idx = *leg_by_call_id.entry(call_id.to_string()).or_insert_with(|| {
            legs.push(FlowLeg {
                call_id: call_id.to_string(),
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
        stats.note_header_failures(i, &msg, &legs[idx].call_id, legs[idx].msgs.len());
        ingest(&mut legs[idx], ts[i], d.src, d.dst, msg, d.probe, &cfg.strategies);
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
    probe: u32,
    strategies: &[CorrelateStrategy],
) {
    for (si, strat) in strategies.iter().enumerate() {
        match strat {
            CorrelateStrategy::HeaderToken { headers } => {
                for name in headers {
                    for v in msg.raw(HeaderName::from(name.as_str())) {
                        let v = v.trim();
                        if !v.is_empty() {
                            leg.tokens_by_strategy[si].insert(v.to_string());
                        }
                    }
                }
            }
            CorrelateStrategy::HeaderParam { header, param } => {
                for v in msg.raw_text(HeaderName::from(header.as_str())) {
                    let params = Params::parse_list(&v);
                    if let Some(t) = params.value(param).filter(|t| !t.is_empty()) {
                        leg.tokens_by_strategy[si].insert(t.to_string());
                    }
                }
            }
            CorrelateStrategy::DerivedCallId { .. }
            | CorrelateStrategy::IdentityAdjacency { .. } => {}
        }
    }
    match &msg {
        SipMessage::Request(r) => {
            if r.method() == Method::Invite && leg.invite.is_none() {
                leg.invite = Some(InviteSummary {
                    ruri: r.request_uri().clone(),
                    from_uri: r.from().uri().clone(),
                    to_uri: r.to().uri().clone(),
                    cseq: r.cseq().seq(),
                });
            }
            if r.method() == Method::Bye && leg.terminated_by.is_none() {
                leg.terminated_by = Some(Terminator::Bye);
            }
            if r.method() == Method::Cancel && leg.terminated_by.is_none() {
                leg.terminated_by = Some(Terminator::Cancel);
            }
        }
        SipMessage::Response(r) => {
            if r.cseq().method() == Method::Invite {
                if r.status() == 180 {
                    leg.saw_180 = true;
                }
                let initial = leg.invite.as_ref().map(|inv| inv.cseq);
                if r.status() >= 200
                    && leg.final_status.is_none()
                    && (initial.is_none() || initial == Some(r.cseq().seq()))
                {
                    leg.final_status = Some(r.status());
                }
            }
        }
    }
    // Retransmission tag: the SAME DATAGRAM already seen in this direction on
    // this leg, inside the transaction envelope. The EARLIEST match anchors it,
    // because 64·T1 runs from the first emission — see [`crate::callfacts`],
    // which decides the same relation for the emitted document and must not be
    // able to disagree with this one.
    let retx = leg
        .msgs
        .iter()
        .find(|m| m.src == src && m.dst == dst && m.parsed.image() == msg.image())
        .is_some_and(|m| ts_us.saturating_sub(m.ts_us) <= crate::callfacts::REPEAT_ENVELOPE_US);
    let pair = Hop::normalized(src, dst);
    let hop = match leg.hops.iter().position(|h| *h == pair) {
        Some(h) => h,
        None => {
            leg.hops.push(pair);
            leg.hops.len() - 1
        }
    };
    leg.msgs.push(FlowMsg { ts_us, src, dst, parsed: msg, hop, retx, probe });
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
/// A later strategy never re-pairs legs an earlier one already joined, but it
/// still records what it independently attests (see the module doc), so
/// evidence lands in pipeline order and a group may carry several
/// corroborating entries. Returns the final group list, ordered by first
/// activity, with the evidence for each group's members.
fn correlate(legs: &[FlowLeg], cfg: &FlowConfig) -> Vec<CallGroup> {
    let mut parent: Vec<usize> = (0..legs.len()).collect();
    let mut evidence: Vec<MatchEvidence> = Vec::new();

    for (si, strat) in cfg.strategies.iter().enumerate() {
        match strat {
            CorrelateStrategy::HeaderToken { .. } | CorrelateStrategy::HeaderParam { .. } => {
                token_pass(si, strat, legs, &mut parent, &mut evidence);
            }
            CorrelateStrategy::DerivedCallId { window_us, require_shared_hop, min_base_len } => {
                derived_call_id_pass(
                    si,
                    *window_us,
                    *require_shared_hop,
                    *min_base_len,
                    legs,
                    &mut parent,
                    &mut evidence,
                );
            }
            CorrelateStrategy::IdentityAdjacency { window_us } => {
                adjacency_pass(si, *window_us, legs, &mut parent, &mut evidence);
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
            MatchEvidence::DerivedCallId { legs, .. }
            | MatchEvidence::IdentityAdjacency { legs, .. } => legs[0],
        };
        let pos = pos_of_root[&find(&mut parent, first_leg)];
        groups[pos].evidence.push(ev);
    }
    groups
}

/// Token-equality pass (strategy `si`): legs sharing a value in
/// `tokens_by_strategy[si]` are one call. Evidence keeps every leg the value
/// was seen on (first-seen value order — deterministic) whether or not this
/// pass is the one that merged them.
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
        // Union only when this strategy actually merges something; the evidence
        // is recorded either way, because a strategy that INDEPENDENTLY attests
        // a pairing an earlier one already made is corroboration, not noise —
        // and a consumer that resolves ambiguity from evidence (which socket of
        // a loopback is the application server) needs every attestation, not
        // just the first.
        let roots: BTreeSet<usize> = ls.iter().map(|&l| find(parent, l)).collect();
        if roots.len() >= 2 {
            for w in ls.windows(2) {
                union(parent, w[0], w[1]);
            }
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
            _ => unreachable!("not a token strategy"),
        });
    }
}

/// `derived = <non-empty prefix> + base` on the Call-ID — the relayed-leg
/// naming convention (`1-`, `term1-`) an application server applies when it
/// re-originates a call it just received.
fn derivation_prefix<'a>(base: &str, derived: &'a str) -> Option<&'a str> {
    (derived.len() > base.len() && derived.ends_with(base))
        .then(|| &derived[..derived.len() - base.len()])
}

/// Call-ID derivation pass (strategy `si`). For every ordered leg pair whose
/// Call-IDs are in the derivation relation, within `window_us` and with the
/// base id at least `min_base_len` long, locate the derived leg's INVITE
/// observation whose socket pair the base leg also traversed: its source is
/// the application server, its destination the peer looped back to. With
/// `require_shared_hop` that observation is mandatory — without it the first
/// INVITE observation stands in and `shared_hop` records which held.
fn derived_call_id_pass(
    si: usize,
    window_us: u64,
    require_shared_hop: bool,
    min_base_len: usize,
    legs: &[FlowLeg],
    parent: &mut [usize],
    evidence: &mut Vec<MatchEvidence>,
) {
    for (base, base_leg) in legs.iter().enumerate() {
        if base_leg.call_id.len() < min_base_len {
            continue;
        }
        for (derived, derived_leg) in legs.iter().enumerate() {
            let Some(prefix) = derivation_prefix(&base_leg.call_id, &derived_leg.call_id) else {
                continue;
            };
            let dt = derived_leg.t_first().abs_diff(base_leg.t_first());
            if dt > window_us {
                continue;
            }
            let looped = derived_leg
                .invite_observations()
                .find(|(s, d)| base_leg.hops.contains(&Hop::normalized(*s, *d)));
            let (as_socket, peer_socket) = match (looped, require_shared_hop) {
                (Some(pair), _) => pair,
                (None, true) => continue,
                (None, false) => match derived_leg.invite_addrs() {
                    Some(pair) => pair,
                    None => continue,
                },
            };
            union(parent, base, derived);
            evidence.push(MatchEvidence::DerivedCallId {
                strategy: si,
                legs: [base, derived],
                prefix: prefix.to_string(),
                as_socket,
                peer_socket,
                shared_hop: looped.is_some(),
                dt_us: dt,
            });
        }
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
    window_us: u64,
    legs: &[FlowLeg],
    parent: &mut [usize],
    evidence: &mut Vec<MatchEvidence>,
) {
    // A leg is eligible iff it is still alone (its group has one member).
    let mut group_size: HashMap<usize, usize> = HashMap::new();
    for i in 0..legs.len() {
        *group_size.entry(find(parent, i)).or_insert(0) += 1;
    }
    let eligible: Vec<bool> = (0..legs.len()).map(|i| group_size[&find(parent, i)] == 1).collect();

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
            if !inv_i.from_uri.same_user_identity(&inv_j.from_uri)
                || !inv_i.to_uri.same_user_identity(&inv_j.to_uri)
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
            if dt <= window_us {
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
mod tests;
