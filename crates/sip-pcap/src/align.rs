//! Probe clock alignment: the offset between the clocks of two probes that
//! wrote the same datagrams, and the rebase that puts every probe of a
//! capture on one clock.
//!
//! A `mergecap` of several probes writes one packet once per probe that saw
//! it, and the copies are stamped by clocks that disagree by an arbitrary
//! offset. The capture-dedup window ([`crate::flow::FlowConfig::dedup_window_us`])
//! collapses a copy only when the offset is inside it; past it, every copy
//! survives as a wire event and reads as a retransmission — of messages that
//! never retransmit (an ACK, RFC 3261 §17.1.1.3; a 2xx to a non-INVITE,
//! §17.2.2) as readily as of those that do. No window can tell such a copy
//! from a T1 rung, because a clock offset lands anywhere in the ladder's own
//! range; the probe id can, because a probe never writes one packet twice.
//!
//! So the offset is MEASURED, per probe pair, from the datagrams both probes
//! wrote — identical in `(src, dst, payload)`, paired rung by rung so a
//! ladder both saw yields the clock's offset and never the ladder's gap — and
//! the later clock is rebased by it. The window then collapses the copies
//! untouched, and the messages only the rebased probe saw land on the
//! reference clock, ordered against everything else.
//!
//! Rank pairing is not proof on its own: two probes can each write a
//! DIFFERENT copy of one retransmitting request — one the first emission,
//! the other the T1 rung — and three such requests state a "clock offset" of
//! exactly T1. So an offset stands only when at least one agreeing shared
//! datagram belongs to a class that [`rides_no_timer`]: an ACK or a response
//! to a non-INVITE written by both probes at the same offset is one packet
//! seen twice and nothing else.
//!
//! A clock also STEPS: a probe adjusted mid-capture reads one offset before
//! the step and another after it. The shared datagrams are therefore read in
//! time order and cut into stretches of agreeing deltas, each stretch its own
//! estimate applying from the first datagram it covers to the next stretch's
//! first (the first stretch from the start), so a pair that agreed to the
//! transit for an hour and then drifted by half a second is aligned on both
//! sides of the step.
//!
//! What is left alone: a stretch of fewer than [`MIN_SHARED_DATAGRAMS`]
//! agreeing datagrams (no estimate to trust), one whose offset the window
//! already absorbs (the copies collapse as they are; the residual is the
//! transit between the vantages, not a clock), one whose shared datagrams all
//! ride a timer, and a shared datagram whose own delta disagrees with its
//! stretch by the window or more (a copy the rebase does not explain stays a
//! wire event).

use std::collections::{BTreeMap, HashMap, VecDeque};

use sip_message::{Method, SipMessage};

use crate::Datagram;

/// How many datagrams two probes must both have written, at one offset,
/// before that offset is a clock's: one or two agree by chance, three state
/// a clock.
pub const MIN_SHARED_DATAGRAMS: usize = 3;

/// One stretch of one probe's clock, rebased onto another's.
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
/// request (§17.2.2). Two copies of one of these on two probes are one
/// packet seen twice. The repeat relation's own view of the classes —
/// which of them can be a REPEAT — is `callfacts`' and wider.
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
    // Per datagram identity, per probe, the datagrams in capture order.
    let mut copies: HashMap<u64, BTreeMap<u32, Vec<usize>>> = HashMap::new();
    let mut order: Vec<usize> = (0..datagrams.len()).filter(|&i| keys[i].is_some()).collect();
    order.sort_by_key(|&i| datagrams[i].ts_us);
    for i in order {
        copies.entry(keys[i].unwrap()).or_default().entry(datagrams[i].probe).or_default().push(i);
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
        all.sort_by_key(|s| s.ts_low);
        let stretches: Vec<Stretch> = runs(&all, window_us)
            .iter()
            .enumerate()
            .map(|(n, run)| stretch(run, window_us, n == 0))
            .collect();
        if stretches.iter().any(|s| s.offset_us != 0) {
            edges.push(Edge { low, high, stretches });
        }
    }
    edges
}

/// The shared datagrams cut into runs of agreeing deltas, in time order. A
/// delta that disagrees with the run so far is an outlier when the next one
/// agrees again, and the start of a new run — a clock step — otherwise.
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

/// One run read as a stretch: its median is the clock's offset when enough
/// datagrams agree with it, one of them rides no timer, and the window does
/// not absorb it already; `0` otherwise. The first run covers the capture
/// from its start.
fn stretch(run: &[Shared], window_us: u64, first: bool) -> Stretch {
    let median = median(run);
    let window = window_us as i64;
    let agreeing: Vec<&Shared> =
        run.iter().filter(|s| (s.delta() - median).abs() < window).collect();
    let states_a_clock = agreeing.len() >= MIN_SHARED_DATAGRAMS
        && agreeing.iter().any(|s| s.anchored)
        && median.abs() >= window;
    Stretch {
        from: if first { (0, 0) } else { (run[0].ts_low, run[0].ts_high) },
        offset_us: if states_a_clock { median } else { 0 },
        pairs: agreeing.len(),
    }
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
mod tests {
    use super::*;

    fn dg(ts_us: u64, probe: u32, payload: &[u8]) -> Datagram {
        Datagram {
            ts_us,
            src: "10.0.0.1:5060".parse().unwrap(),
            dst: "10.0.0.2:5060".parse().unwrap(),
            payload: payload.to_vec(),
            probe,
        }
    }

    fn keys(datagrams: &[Datagram]) -> Vec<Option<u64>> {
        datagrams.iter().map(|d| Some(d.payload[0] as u64)).collect()
    }

    /// The datagram whose payload is `a` stands for the ACK of the exchange.
    fn anchored(datagrams: &[Datagram]) -> impl Fn(usize) -> bool + '_ {
        move |i| datagrams[i].payload == b"a"
    }

    fn offset(
        probe: u32,
        reference: u32,
        from_us: u64,
        offset_us: i64,
        pairs: usize,
    ) -> ProbeOffset {
        ProbeOffset { probe, reference, from_us, offset_us, pairs }
    }

    /// Three shared datagrams at one offset: the later clock is rebased, its
    /// unshared datagram with it.
    #[test]
    fn a_constant_offset_rebases_the_later_probe() {
        let datagrams = vec![
            dg(1_000_000, 0, b"a"),
            dg(1_300_000, 1, b"a"),
            dg(2_000_000, 0, b"b"),
            dg(2_310_000, 1, b"b"),
            dg(3_000_000, 0, b"c"),
            dg(3_290_000, 1, b"c"),
            dg(4_300_000, 1, b"d"),
        ];
        let (ts, applied) =
            align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
        assert_eq!(
            ts,
            vec![1_000_000, 1_000_000, 2_000_000, 2_010_000, 3_000_000, 2_990_000, 4_000_000]
        );
        assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 3)]);
    }

    /// The earlier CLOCK is the reference whatever the probe numbering.
    #[test]
    fn the_reference_is_the_earliest_clock() {
        let datagrams = vec![
            dg(1_300_000, 0, b"a"),
            dg(1_000_000, 1, b"a"),
            dg(2_300_000, 0, b"b"),
            dg(2_000_000, 1, b"b"),
            dg(3_300_000, 0, b"c"),
            dg(3_000_000, 1, b"c"),
        ];
        let (_, applied) =
            align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
        assert_eq!(applied, vec![offset(0, 1, 0, 300_000, 3)]);
    }

    /// A delta the offset does not explain neither shifts the estimate nor
    /// counts toward it, and does not end the stretch.
    #[test]
    fn an_outlier_delta_is_left_out_of_the_estimate() {
        let datagrams = vec![
            dg(1_000_000, 0, b"a"),
            dg(1_300_000, 1, b"a"),
            dg(2_000_000, 0, b"b"),
            dg(2_300_000, 1, b"b"),
            dg(2_500_000, 0, b"d"),
            dg(7_500_000, 1, b"d"),
            dg(3_000_000, 0, b"c"),
            dg(3_300_000, 1, b"c"),
        ];
        let (_, applied) =
            align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
        assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 3)]);
    }

    /// Two shared datagrams are no estimate.
    #[test]
    fn too_few_shared_datagrams_state_no_offset() {
        let datagrams = vec![
            dg(1_000_000, 0, b"a"),
            dg(1_300_000, 1, b"a"),
            dg(2_000_000, 0, b"b"),
            dg(2_300_000, 1, b"b"),
        ];
        let (ts, applied) =
            align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
        assert!(applied.is_empty());
        assert_eq!(ts, vec![1_000_000, 1_300_000, 2_000_000, 2_300_000]);
    }

    /// Three shared datagrams that all ride a timer state no offset: two
    /// probes each writing a different copy of a retransmitting request agree
    /// at T1 as tightly as two clocks do.
    #[test]
    fn shared_datagrams_that_all_ride_a_timer_state_no_offset() {
        let datagrams = vec![
            dg(1_000_000, 0, b"b"),
            dg(1_500_000, 1, b"b"),
            dg(2_000_000, 0, b"c"),
            dg(2_500_000, 1, b"c"),
            dg(3_000_000, 0, b"d"),
            dg(3_500_000, 1, b"d"),
        ];
        let (_, applied) =
            align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
        assert!(applied.is_empty());
    }

    /// A clock step splits the pair into two stretches, each estimated on its
    /// own and applied from the first datagram it covers.
    #[test]
    fn a_clock_step_makes_two_stretches() {
        let datagrams = vec![
            dg(1_000_000, 0, b"a"),
            dg(1_005_000, 1, b"a"),
            dg(2_000_000, 0, b"b"),
            dg(2_005_000, 1, b"b"),
            dg(3_000_000, 0, b"c"),
            dg(3_005_000, 1, b"c"),
            dg(4_000_000, 0, b"a"),
            dg(4_405_000, 1, b"a"),
            dg(5_000_000, 0, b"b"),
            dg(5_405_000, 1, b"b"),
            dg(6_000_000, 0, b"c"),
            dg(6_405_000, 1, b"c"),
            dg(6_900_000, 1, b"e"),
        ];
        let (ts, applied) =
            align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
        assert_eq!(applied, vec![offset(1, 0, 0, 0, 3), offset(1, 0, 4_405_000, 405_000, 3)]);
        assert_eq!(ts[1], 1_005_000, "the first stretch is left on its clock");
        assert_eq!(ts[7], 4_000_000);
        assert_eq!(ts[12], 6_495_000, "the probe's own datagram after the step moves with it");
    }

    /// Datagrams without a key take no part.
    #[test]
    fn unkeyed_datagrams_take_no_part() {
        let datagrams = vec![
            dg(1_000_000, 0, b"a"),
            dg(1_300_000, 1, b"a"),
            dg(2_000_000, 0, b"b"),
            dg(2_300_000, 1, b"b"),
            dg(3_000_000, 0, b"c"),
            dg(3_300_000, 1, b"c"),
        ];
        let mut k = keys(&datagrams);
        k[4] = None;
        k[5] = None;
        let (_, applied) = align_probes(&datagrams, &k, 200_000, anchored(&datagrams));
        assert!(applied.is_empty());
    }
}
