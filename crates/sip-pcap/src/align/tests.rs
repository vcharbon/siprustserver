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

fn offset(probe: u32, reference: u32, from_us: u64, offset_us: i64, pairs: usize) -> ProbeOffset {
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
    let (ts, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
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
    let (_, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
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
    let (_, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
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
    let (ts, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
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
    let (_, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
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
    let (ts, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
    assert_eq!(applied, vec![offset(1, 0, 0, 0, 3), offset(1, 0, 4_405_000, 405_000, 3)]);
    assert_eq!(ts[1], 1_005_000, "the first stretch is left on its clock");
    assert_eq!(ts[7], 4_000_000);
    assert_eq!(ts[12], 6_495_000, "the probe's own datagram after the step moves with it");
}

/// A disagreeing delta at the END of the pair is an outlier like any other:
/// the stretch before it stays in force and the probe's own datagrams after
/// it stay on the reference clock.
#[test]
fn a_trailing_outlier_does_not_end_the_stretch() {
    let datagrams = vec![
        dg(1_000_000, 0, b"a"),
        dg(1_300_000, 1, b"a"),
        dg(2_000_000, 0, b"b"),
        dg(2_300_000, 1, b"b"),
        dg(3_000_000, 0, b"c"),
        dg(3_300_000, 1, b"c"),
        dg(4_000_000, 0, b"d"),
        dg(4_800_000, 1, b"d"),
        dg(4_900_000, 1, b"e"),
    ];
    let (ts, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
    assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 3)]);
    assert_eq!(ts[8], 4_600_000);
}

/// Two disagreeing deltas back to back (one probe missed the first copy of
/// a repeated request) neither state a clock nor drop the one in force;
/// the agreeing datagrams after them extend the stretch.
#[test]
fn a_rank_shift_does_not_open_a_zero_stretch() {
    let datagrams = vec![
        dg(1_000_000, 0, b"a"),
        dg(1_300_000, 1, b"a"),
        dg(2_000_000, 0, b"b"),
        dg(2_300_000, 1, b"b"),
        dg(3_000_000, 0, b"c"),
        dg(3_300_000, 1, b"c"),
        dg(4_000_000, 0, b"d"),
        dg(4_500_000, 0, b"d"),
        dg(5_500_000, 0, b"d"),
        dg(4_800_000, 1, b"d"),
        dg(5_800_000, 1, b"d"),
        dg(6_000_000, 1, b"e"),
        dg(7_000_000, 0, b"a"),
        dg(7_300_000, 1, b"a"),
        dg(8_000_000, 0, b"b"),
        dg(8_300_000, 1, b"b"),
        dg(9_000_000, 0, b"c"),
        dg(9_300_000, 1, b"c"),
    ];
    let (ts, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
    assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 6)]);
    assert_eq!(ts[11], 5_700_000);
}

/// A leading outlier does not delay the first stretch: it applies from the
/// start.
#[test]
fn a_leading_outlier_does_not_delay_the_stretch() {
    let datagrams = vec![
        dg(500_000, 0, b"d"),
        dg(1_300_000, 1, b"d"),
        dg(600_000, 1, b"e"),
        dg(1_000_000, 0, b"a"),
        dg(1_300_000, 1, b"a"),
        dg(2_000_000, 0, b"b"),
        dg(2_300_000, 1, b"b"),
        dg(3_000_000, 0, b"c"),
        dg(3_300_000, 1, b"c"),
    ];
    let (ts, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
    assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 3)]);
    assert_eq!(ts[2], 300_000);
}

/// A step back INTO the window is a genuine zero: three agreeing deltas
/// with an anchor state it.
#[test]
fn a_step_back_inside_the_window_states_zero() {
    let datagrams = vec![
        dg(1_000_000, 0, b"a"),
        dg(1_300_000, 1, b"a"),
        dg(2_000_000, 0, b"b"),
        dg(2_300_000, 1, b"b"),
        dg(3_000_000, 0, b"c"),
        dg(3_300_000, 1, b"c"),
        dg(4_000_000, 0, b"a"),
        dg(4_005_000, 1, b"a"),
        dg(5_000_000, 0, b"b"),
        dg(5_005_000, 1, b"b"),
        dg(6_000_000, 0, b"c"),
        dg(6_005_000, 1, b"c"),
    ];
    let (_, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
    assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 3), offset(1, 0, 4_005_000, 0, 3)]);
}

/// A step whose second run cannot state a clock leaves the offset in
/// force where it is.
#[test]
fn a_step_too_thin_to_read_keeps_the_offset_in_force() {
    let datagrams = vec![
        dg(1_000_000, 0, b"a"),
        dg(1_300_000, 1, b"a"),
        dg(2_000_000, 0, b"b"),
        dg(2_300_000, 1, b"b"),
        dg(3_000_000, 0, b"c"),
        dg(3_300_000, 1, b"c"),
        dg(4_000_000, 0, b"a"),
        dg(4_700_000, 1, b"a"),
        dg(5_000_000, 0, b"b"),
        dg(5_700_000, 1, b"b"),
        dg(6_000_000, 1, b"e"),
    ];
    let (ts, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
    assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 3)]);
    assert_eq!(ts[10], 5_700_000);
}

/// A probe writing one packet twice inside the window (`-i any`: veth and
/// bridge) ranks it once, so its second stack copy never pairs with the
/// other probe's T1 rung.
#[test]
fn same_probe_stack_copies_rank_once() {
    let datagrams = vec![
        dg(1_000_000, 0, b"a"),
        dg(1_000_050, 0, b"a"),
        dg(1_300_000, 1, b"a"),
        dg(2_000_000, 0, b"b"),
        dg(2_000_050, 0, b"b"),
        dg(2_300_000, 1, b"b"),
        dg(2_500_000, 0, b"b"),
        dg(2_500_050, 0, b"b"),
        dg(2_800_000, 1, b"b"),
        dg(3_000_000, 0, b"c"),
        dg(3_000_050, 0, b"c"),
        dg(3_300_000, 1, b"c"),
    ];
    let (_, applied) = align_probes(&datagrams, &keys(&datagrams), 200_000, anchored(&datagrams));
    assert_eq!(applied, vec![offset(1, 0, 0, 300_000, 4)]);
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
