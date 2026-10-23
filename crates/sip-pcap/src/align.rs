//! Probe clock alignment: the offset between the clocks of two probes that
//! wrote the same datagrams, and the rebase that puts every probe of a
//! capture on one clock before the capture dedup
//! ([`crate::flow::FlowConfig::dedup_window_us`]).
//!
//! Measured per probe pair on the datagrams both probes wrote (identical
//! `(src, dst, payload)`, the k-th copy on one probe against the k-th on the
//! other), read in time order and cut into stretches of deltas agreeing
//! within the window. A stretch states a clock on at least
//! [`MIN_SHARED_DATAGRAMS`] agreeing deltas, one of them of a class that
//! [`rides_no_timer`]: its median, or zero where the window absorbs it. A
//! run that states no clock is folded into the stretch before it — the offset
//! in force stays, its own copies stay wire events — and dropped when nothing
//! precedes it. Probes hang under the earliest clock of their connected
//! component. Why a window cannot do this, why rank pairing needs the
//! no-timer class and why a clock steps is `docs/sipflow.md`'s.

use std::collections::{BTreeMap, HashMap, VecDeque};

use sip_message::{Method, SipMessage};

use crate::Datagram;

/// How many datagrams two probes must both have written, at one offset,
/// before that offset is a clock's: one or two agree by chance, three state
/// a clock.
pub const MIN_SHARED_DATAGRAMS: usize = 3;

/// One stretch of one probe's clock read against another's — every stretch
/// of an aligned probe, a zero one (the clock back inside the window)
/// included, so the reader sees where the steps were.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeOffset {
    /// The probe whose timestamps were moved.
    pub probe: u32,
    /// The probe whose clock it now shares: the earliest clock of the probes
    /// it is connected to through shared datagrams.
    pub reference: u32,
    /// The timestamp `probe` wrote from which this offset applies (its own
    /// clock); `0` for the first stretch. It applies until the probe's next
    /// stretch begins.
    pub from_us: u64,
    /// Microseconds subtracted from every timestamp `probe` wrote in the
    /// stretch — negative when this probe's clock ran behind the reference's.
    pub offset_us: i64,
    /// Shared datagrams whose delta agreed with the offset, the estimate's
    /// weight.
    pub pairs: usize,
}

/// Whether a message belongs to a class RFC 3261 gives no retransmission
/// timer of its own: an ACK (§17.1.1.3), or a response to a non-INVITE
/// request (§17.2.2). The class still echoes a repeated request — a 200 to a
/// retransmitted BYE, the ACK to each 2xx retransmission (§13.2.2.4) — so two
/// copies on two probes are one packet seen twice unless the request they
/// echo was itself split between the probes. The repeat relation's own view
/// of the classes — which of them can be a REPEAT — is `callfacts`' and wider.
pub fn rides_no_timer(msg: &SipMessage) -> bool {
    match msg {
        SipMessage::Request(r) => r.method() == Method::Ack,
        SipMessage::Response(r) => r.cseq().method() != Method::Invite,
    }
}

/// The per-datagram timestamps once every probe is on its component's
/// earliest clock, and the offsets applied. `keys[i]` is the dedup identity of
/// `datagrams[i]` (`(src, dst, payload)`), `None` for a datagram that takes no
/// part in the estimate; `anchor(i)` says whether `datagrams[i]` is of a class
/// that [`rides_no_timer`], asked only of datagrams two probes both wrote.
pub fn align_probes(
    datagrams: &[Datagram],
    keys: &[Option<u64>],
    window_us: u64,
    anchor: impl Fn(usize) -> bool,
) -> (Vec<u64>, Vec<ProbeOffset>) {
    let edges = pair_offsets(datagrams, keys, window_us, anchor);
    let tree = Tree::spanning(&edges);
    let ts = datagrams
        .iter()
        .map(|d| (d.ts_us as i64 - tree.position(d.probe, d.ts_us)).max(0) as u64)
        .collect();
    (ts, tree.report())
}

/// One stretch of a probe pair: from `from` (`.0` on `low`'s clock, `.1` on
/// `high`'s), `high` reads `offset_us` ahead of `low` on `pairs` agreeing
/// shared datagrams — `0` where the stretch states no clock.
#[derive(Debug, Clone, Copy)]
struct Stretch {
    from: (u64, u64),
    offset_us: i64,
    pairs: usize,
}

/// A probe pair with at least one stretch that states a clock.
#[derive(Debug, Clone)]
struct Edge {
    low: u32,
    high: u32,
    stretches: Vec<Stretch>,
}

/// One datagram both probes of a pair wrote: the copies' timestamps and
/// whether the class rides no timer.
#[derive(Debug, Clone, Copy)]
struct Shared {
    ts_low: u64,
    ts_high: u64,
    anchored: bool,
}

impl Shared {
    fn delta(&self) -> i64 {
        self.ts_high as i64 - self.ts_low as i64
    }
}

/// Every probe pair with a trustworthy offset outside the window somewhere
/// along the capture.
fn pair_offsets(
    datagrams: &[Datagram],
    keys: &[Option<u64>],
    window_us: u64,
    anchor: impl Fn(usize) -> bool,
) -> Vec<Edge> {
    // Per datagram identity, per probe, the datagrams in capture order — a
    // probe's own stack copy inside the window (`-i any`: veth and bridge)
    // ranks once, as the dedup will read it.
    let mut copies: HashMap<u64, BTreeMap<u32, Vec<usize>>> = HashMap::new();
    let mut order: Vec<usize> = (0..datagrams.len()).filter(|&i| keys[i].is_some()).collect();
    order.sort_by_key(|&i| (datagrams[i].ts_us, i));
    for i in order {
        let ranked =
            copies.entry(keys[i].unwrap()).or_default().entry(datagrams[i].probe).or_default();
        if ranked.last().is_none_or(|&j| datagrams[i].ts_us - datagrams[j].ts_us >= window_us) {
            ranked.push(i);
        }
    }
    // Per probe pair (low, high), every shared datagram: the k-th copy on one
    // probe against the k-th on the other.
    let mut shared: BTreeMap<(u32, u32), Vec<Shared>> = BTreeMap::new();
    for by_probe in copies.values() {
        if by_probe.len() < 2 {
            continue;
        }
        let anchored = anchor(by_probe.values().next().unwrap()[0]);
        let probes: Vec<(&u32, &Vec<usize>)> = by_probe.iter().collect();
        for (a, (pa, ia)) in probes.iter().enumerate() {
            for (pb, ib) in &probes[a + 1..] {
                let pair = shared.entry((**pa, **pb)).or_default();
                for (x, y) in ia.iter().zip(ib.iter()) {
                    pair.push(Shared {
                        ts_low: datagrams[*x].ts_us,
                        ts_high: datagrams[*y].ts_us,
                        anchored,
                    });
                }
            }
        }
    }
    let mut edges = Vec::new();
    for ((low, high), mut all) in shared {
        all.sort_by_key(|s| (s.ts_low, s.ts_high));
        let stretches = stretches(&runs(&all, window_us), window_us);
        if stretches.iter().any(|s| s.offset_us != 0) {
            edges.push(Edge { low, high, stretches });
        }
    }
    edges
}

/// The shared datagrams cut into runs of agreeing deltas, in time order. A
/// delta that disagrees with the run so far is left out when the next one
/// agrees again, and starts a new run otherwise.
fn runs(shared: &[Shared], window_us: u64) -> Vec<Vec<Shared>> {
    let agrees = |run: &[Shared], s: &Shared| (s.delta() - median(run)).abs() < window_us as i64;
    let mut runs: Vec<Vec<Shared>> = Vec::new();
    let mut run: Vec<Shared> = Vec::new();
    for (i, s) in shared.iter().enumerate() {
        if run.is_empty() || agrees(&run, s) {
            run.push(*s);
        } else if !shared.get(i + 1).is_some_and(|next| agrees(&run, next)) {
            runs.push(std::mem::take(&mut run));
            run.push(*s);
        }
    }
    if !run.is_empty() {
        runs.push(run);
    }
    runs
}

fn median(run: &[Shared]) -> i64 {
    let mut ds: Vec<i64> = run.iter().map(Shared::delta).collect();
    ds.sort_unstable();
    ds[ds.len() / 2]
}

/// The runs read as stretches. A run states a clock — its median, or zero
/// where the window absorbs it — on [`MIN_SHARED_DATAGRAMS`] agreeing deltas
/// at least, one of them riding no timer; a run stating none is folded into
/// the stretch before it and dropped when nothing precedes it. A run stating
/// the clock already in force extends that stretch. The first stretch covers
/// the capture from its start.
fn stretches(runs: &[Vec<Shared>], window_us: u64) -> Vec<Stretch> {
    let window = window_us as i64;
    let mut out: Vec<Stretch> = Vec::new();
    for run in runs {
        let median = median(run);
        let agreeing = run.iter().filter(|s| (s.delta() - median).abs() < window);
        let (pairs, anchored) = agreeing.fold((0, false), |(n, a), s| (n + 1, a || s.anchored));
        if pairs < MIN_SHARED_DATAGRAMS || !anchored {
            continue;
        }
        let offset_us = if median.abs() < window { 0 } else { median };
        match out.last_mut() {
            Some(last) if (last.offset_us - offset_us).abs() < window => last.pairs += pairs,
            Some(_) => {
                out.push(Stretch { from: (run[0].ts_low, run[0].ts_high), offset_us, pairs })
            }
            None => out.push(Stretch { from: (0, 0), offset_us, pairs }),
        }
    }
    out
}

/// One stretch as a probe reads it against its parent in the spanning tree:
/// from `from_us` on the probe's own clock, the probe reads `offset_us`
/// ahead of the parent.
#[derive(Debug, Clone, Copy)]
struct Oriented {
    from_us: u64,
    offset_us: i64,
    pairs: usize,
}

/// Every probe an edge touches, hung under the earliest clock of its
/// connected component: a probe's position against the reference composes
/// along the path the tree reaches it by.
#[derive(Debug, Default)]
struct Tree {
    /// Probe → (its parent, its reference, the stretches read from its side).
    nodes: HashMap<u32, (u32, u32, Vec<Oriented>)>,
}

impl Tree {
    fn spanning(edges: &[Edge]) -> Tree {
        let mut adjacent: BTreeMap<u32, Vec<&Edge>> = BTreeMap::new();
        for e in edges {
            adjacent.entry(e.low).or_default().push(e);
            adjacent.entry(e.high).or_default().push(e);
        }
        // Breadth-first from `from`, each probe placed by the first edge that
        // reaches it: the probes of the component with their placing edge.
        let walk = |from: u32| -> Vec<(u32, Option<(u32, &Edge)>)> {
            let mut placed: Vec<(u32, Option<(u32, &Edge)>)> = vec![(from, None)];
            let mut queue = VecDeque::from([from]);
            while let Some(p) = queue.pop_front() {
                for e in &adjacent[&p] {
                    let other = if e.low == p { e.high } else { e.low };
                    if placed.iter().any(|(q, _)| *q == other) {
                        continue;
                    }
                    placed.push((other, Some((p, e))));
                    queue.push_back(other);
                }
            }
            placed
        };
        let mut tree = Tree::default();
        let mut placed: Vec<u32> = Vec::new();
        for &seed in adjacent.keys() {
            if placed.contains(&seed) {
                continue;
            }
            // The reference is the earliest clock at the first offset each
            // edge states, composed from the seed.
            let component = walk(seed);
            let mut at: HashMap<u32, i64> = HashMap::from([(seed, 0)]);
            for (probe, parent) in &component {
                if let Some((parent, e)) = parent {
                    let first = e.stretches.iter().find(|s| s.offset_us != 0).unwrap().offset_us;
                    let signed = if *probe == e.high { first } else { -first };
                    at.insert(*probe, at[parent] + signed);
                }
            }
            let reference = component.iter().map(|(p, _)| *p).min_by_key(|p| (at[p], *p)).unwrap();
            for (probe, parent) in walk(reference) {
                placed.push(probe);
                let Some((parent, e)) = parent else { continue };
                let stretches = e
                    .stretches
                    .iter()
                    .map(|s| Oriented {
                        from_us: if probe == e.high { s.from.1 } else { s.from.0 },
                        offset_us: if probe == e.high { s.offset_us } else { -s.offset_us },
                        pairs: s.pairs,
                    })
                    .collect();
                tree.nodes.insert(probe, (parent, reference, stretches));
            }
        }
        tree
    }

    /// Signed microseconds `probe`'s clock reads ahead of its reference at
    /// `ts_us` on its own clock; `0` for a probe on no edge or the reference.
    fn position(&self, probe: u32, ts_us: u64) -> i64 {
        let Some((parent, _, stretches)) = self.nodes.get(&probe) else { return 0 };
        let own = stretches.iter().rev().find(|s| s.from_us <= ts_us).map_or(0, |s| s.offset_us);
        own + self.position(*parent, (ts_us as i64 - own).max(0) as u64)
    }

    fn report(&self) -> Vec<ProbeOffset> {
        let mut out: Vec<ProbeOffset> = self
            .nodes
            .iter()
            .flat_map(|(probe, (_, reference, stretches))| {
                stretches.iter().map(|s| ProbeOffset {
                    probe: *probe,
                    reference: *reference,
                    from_us: s.from_us,
                    offset_us: self.position(*probe, s.from_us),
                    pairs: s.pairs,
                })
            })
            .collect();
        out.sort_by_key(|o| (o.probe, o.from_us));
        out
    }
}

#[cfg(test)]
mod tests;
