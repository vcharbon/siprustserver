//! The interface is the test surface: one table over every class, so the five
//! sites that used to write this arithmetic each cannot drift apart again.

use super::*;

const fn m(v: u64) -> Duration {
    Duration::from_millis(v)
}

/// One row per class: the first four rung intervals, the interval a long
/// ladder settles at (rung 12 — past every cap), and the give-up.
#[test]
fn every_class_paces_and_bounds_as_its_rfc_states() {
    let rows: &[(Class, [u64; 4], u64, u64)] = &[
        // §17.1.1.2 Timer A: doubling, no ceiling, Timer B.
        (
            Class::InviteClient,
            [500, 1000, 2000, 4000],
            500 * 2048,
            32_000,
        ),
        // §17.1.2.2 Timer E in Trying: doubling, T2 ceiling, Timer F.
        (
            Class::NonInviteClient,
            [500, 1000, 2000, 4000],
            4000,
            32_000,
        ),
        // §17.1.2.2 in Proceeding: flat T2 from the first fire after it.
        (
            Class::NonInviteProceeding,
            [4000, 4000, 4000, 4000],
            4000,
            32_000,
        ),
        // ADR-0028 X4: a non-INVITE's pacing, its own 64·T1 ceiling.
        (Class::CancelClient, [500, 1000, 2000, 4000], 4000, 32_000),
        // §17.2.1 Timer G: doubling, T2 ceiling, until the ACK or Timer H.
        (
            Class::InviteServerFinal,
            [500, 1000, 2000, 4000],
            4000,
            32_000,
        ),
        // §13.3.1.4: doubling, T2 ceiling, until the ACK or Timer L.
        (Class::Final2xx, [500, 1000, 2000, 4000], 4000, 32_000),
        // RFC 3262 §3: doubling, NO ceiling — a PRACK is a request of its own.
        (
            Class::ReliableProvisional,
            [500, 1000, 2000, 4000],
            500 * 2048,
            32_000,
        ),
    ];

    for (class, first_four, settled, give_up) in rows {
        let s = Schedule::rfc(*class);
        for (i, want) in first_four.iter().enumerate() {
            assert_eq!(
                s.interval(i as u32 + 1),
                Some(m(*want)),
                "{class:?} rung {}",
                i + 1
            );
        }
        assert_eq!(
            s.interval(12),
            Some(m(*settled)),
            "{class:?} settled interval"
        );
        assert_eq!(s.give_up_after(), m(*give_up), "{class:?} give-up");
    }
}

#[test]
fn rung_zero_is_the_original_send_and_has_no_wait() {
    let s = Schedule::rfc(Class::Final2xx);
    assert_eq!(s.interval(0), None);
    assert_eq!(s.elapsed_at(0), Duration::ZERO);
}

/// The elapsed total is what makes a rung index the whole of a ladder's state.
#[test]
fn elapsed_is_the_sum_of_the_rungs_before_it() {
    let s = Schedule::rfc(Class::InviteClient);
    assert_eq!(s.elapsed_at(1), m(500));
    assert_eq!(s.elapsed_at(2), m(1500));
    assert_eq!(s.elapsed_at(3), m(3500));
    // The cadence ADR-0007 X1 states: sends at +500 / +1500 / +3500 ms.
}

/// A takeover restores a rung index alone; the ladder it rebuilds is the one
/// that was running.
#[test]
fn a_ladder_rebuilt_from_its_rung_stands_where_it_stood() {
    let (mut walked, _) = Ladder::armed(Schedule::rfc(Class::ReliableProvisional)).unwrap();
    walked.advance().unwrap();
    walked.advance().unwrap();

    let restored = Ladder::at_rung(Schedule::rfc(Class::ReliableProvisional), walked.rung());
    assert_eq!(restored, walked);
    assert_eq!(restored.elapsed(), m(3500));
}

/// Walk a ladder to exhaustion: the rung it stops on, and the time it stops at.
fn walk(schedule: Schedule) -> (u32, Duration) {
    let (mut l, _) = Ladder::armed(schedule).expect("a retransmitting class");
    while l.advance().is_some() {
        assert!(l.rung() < 100, "a bounded ladder must stop");
    }
    (l.rung(), l.elapsed())
}

#[test]
fn a_ladder_stops_before_a_rung_that_would_land_at_or_past_the_give_up() {
    // 500 + 1000 + 2000 + 4000, then T2 forever: rung 10 lands at 31 500 ms and
    // rung 11 would land at 35 500, past Timer H.
    assert_eq!(walk(Schedule::rfc(Class::InviteServerFinal)), (10, m(31_500)));
}

/// The bound is crossed by landing ON it, not only past it. Every RFC class
/// steps either side of its own bound, so only a configured one pins the
/// comparison — and `invite_initial_timeout_ms` is exactly such a bound.
#[test]
fn a_rung_landing_exactly_on_the_bound_is_not_sent() {
    // Rungs at 500 / 1500 / 3500 / 7500 ms; the next lands on 15 500 exactly.
    let tight = Schedule::rfc(Class::InviteClient).with_give_up(m(15_500));
    assert_eq!(walk(tight), (4, m(7_500)));

    // One millisecond of room, and that rung is owed after all.
    let looser = Schedule::rfc(Class::InviteClient).with_give_up(m(15_501));
    assert_eq!(walk(looser), (5, m(15_500)));
}

/// The `Trying` → `Proceeding` switch changes the pace, and keeps both the
/// elapsed total and the bound.
#[test]
fn retargeting_changes_the_pace_and_keeps_the_elapsed() {
    let (mut l, _) =
        Ladder::armed(Schedule::rfc(Class::NonInviteClient).with_give_up(m(9_000))).unwrap();
    assert_eq!(l.advance(), Some(m(1000)));
    assert_eq!(l.elapsed(), m(1500));

    l.retarget(Class::NonInviteProceeding);
    assert_eq!(l.give_up_after(), m(9_000), "re-pacing moved the deadline");
    assert_eq!(
        l.advance(),
        Some(m(4000)),
        "Proceeding re-arms at exactly T2"
    );
    assert_eq!(l.elapsed(), m(5500), "the switch did not reset the clock");
}

/// The owner's own bound replaces the class's, in either direction: a
/// deployment may configure an INVITE bound below Timer B, and the tighter
/// deadline owns the give-up.
#[test]
fn an_owners_bound_replaces_the_classs() {
    let raised = Schedule::rfc(Class::InviteClient).with_give_up(m(158_000));
    assert_eq!(raised.give_up_after(), m(158_000));

    let tightened = Schedule::rfc(Class::InviteClient).with_give_up(m(10_000));
    assert_eq!(tightened.give_up_after(), m(10_000));

    assert_eq!(walk(tightened), (4, m(7_500)), "the 15.5 s rung is past 10 s");
}

/// A deadline tightens the bound or leaves it: a 60 s ACK policy does not
/// carry the 2xx ladder past Timer L, a 10 s one cuts it short.
#[test]
fn a_tightened_bound_never_passes_the_classs() {
    let past_timer_l = Schedule::rfc(Class::Final2xx).tightened_to(m(60_000));
    assert_eq!(past_timer_l.give_up_after(), m(32_000), "Timer L stands");
    // Rungs at 0.5 / 1.5 / 3.5 / 7.5 / 11.5 / 15.5 / 19.5 / 23.5 / 27.5 / 31.5 s;
    // the eleventh would land on 35.5 s, past Timer L.
    assert_eq!(walk(past_timer_l), (10, m(31_500)));

    let sooner = Schedule::rfc(Class::Final2xx).tightened_to(m(10_000));
    assert_eq!(sooner.give_up_after(), m(10_000));
    assert_eq!(walk(sooner), (4, m(7_500)), "the 11.5 s rung is past 10 s");
}

#[test]
fn a_class_is_readable_back_off_its_schedule() {
    assert_eq!(
        Schedule::rfc(Class::Final2xx).class(),
        Some(Class::Final2xx)
    );
}

/// One stable label per class, distinct across the family, in the kebab-case
/// a Prometheus label carries — and `ALL` names each class exactly once.
#[test]
fn every_class_has_a_distinct_stable_label() {
    let labels: Vec<&str> = Class::ALL.iter().map(|c| c.as_str()).collect();
    assert_eq!(
        labels,
        [
            "invite-client",
            "non-invite-client",
            "non-invite-proceeding",
            "cancel-client",
            "invite-server-final",
            "final-2xx",
            "reliable-provisional",
        ]
    );
    let mut sorted = labels.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), labels.len(), "no two classes share a label");
}

/// A retargeted ladder names the class it now paces by.
#[test]
fn a_ladder_names_the_class_pacing_it() {
    let (mut ladder, _) = Ladder::armed(Schedule::rfc(Class::NonInviteClient)).unwrap();
    assert_eq!(ladder.class(), Some(Class::NonInviteClient));
    ladder.retarget(Class::NonInviteProceeding);
    assert_eq!(ladder.class(), Some(Class::NonInviteProceeding));
}

#[cfg(feature = "authored")]
mod authored {
    use super::*;

    /// A captured platform's ladder is not the RFC's — measured across the
    /// corpus it runs from 409 ms to 17.5 s where T1 says 500.
    #[test]
    fn an_exact_schedule_paces_by_the_gaps_the_wire_measured() {
        let s = Schedule::exact(&[m(479), m(2100)], None);
        assert_eq!(s.interval(1), Some(m(479)));
        assert_eq!(s.interval(2), Some(m(2100)));
        assert_eq!(s.give_up_after(), m(2579), "its list is its bound");
    }

    #[test]
    fn an_exact_schedule_past_its_list_repeats_its_last_gap() {
        let s = Schedule::exact(&[m(479), m(2100)], Some(m(60_000)));
        assert_eq!(s.interval(3), Some(m(2100)));
        assert_eq!(s.interval(9), Some(m(2100)));
    }

    /// `once` is not `rfc` with an empty list: an ACK and an unreliable
    /// provisional ride no timer at all, and saying so is its own statement.
    #[test]
    fn once_has_no_rung_and_is_no_class() {
        let s = Schedule::once();
        assert_eq!(s.interval(1), None);
        assert_eq!(s.class(), None);
        assert!(Ladder::armed(s.clone()).is_none());
        // "sent once" and "paced by the RFC" are different statements, and so
        // are "sent once" and "an authored ladder with no gaps".
        assert_ne!(s, Schedule::rfc(Class::InviteClient));
        assert_ne!(s, Schedule::exact(&[], None));
    }

    #[test]
    fn an_authored_schedule_carries_the_bound_it_was_given() {
        let s = Schedule::exact(&[m(1000)], Some(m(5_000)));
        assert_eq!(walk(s), (4, m(4_000)), "the 5 s rung lands on the bound");
    }
}
