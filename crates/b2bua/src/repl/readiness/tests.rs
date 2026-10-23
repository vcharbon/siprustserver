use super::*;
use std::sync::atomic::AtomicBool;

/// A flip-able source for truth-table + transition tests.
struct FlagSource {
    bootstrapped: AtomicBool,
    current: AtomicBool,
}
impl FlagSource {
    fn new(b: bool, c: bool) -> Arc<Self> {
        Arc::new(Self { bootstrapped: AtomicBool::new(b), current: AtomicBool::new(c) })
    }
    fn set(&self, b: bool, c: bool) {
        self.bootstrapped.store(b, Ordering::SeqCst);
        self.current.store(c, Ordering::SeqCst);
    }
}
impl ReadinessSource for FlagSource {
    fn all_bootstrapped(&self) -> bool {
        self.bootstrapped.load(Ordering::SeqCst)
    }
    fn all_current(&self) -> bool {
        self.current.load(Ordering::SeqCst)
    }
}

#[test]
fn truth_table_before_latch() {
    // (bootstrapped, current) → state, with a fresh (un-latched) Readiness.
    for (b, c, want) in [
        (false, false, ReadinessState::NotReady),
        (true, false, ReadinessState::NotReady),
        (false, true, ReadinessState::NotReady),
        (true, true, ReadinessState::Ready),
    ] {
        let r = Readiness::new(FlagSource::new(b, c));
        assert_eq!(r.state(), want, "({b},{c})");
    }
}

/// A source whose gates are vacuously true over an UNSYNCED membership view
/// (the informer's empty boot snapshot) must NOT latch Ready: peers — and
/// the calls to reclaim from them — are not visible yet.
struct UnsyncedSource {
    synced: AtomicBool,
}
impl ReadinessSource for UnsyncedSource {
    fn all_bootstrapped(&self) -> bool {
        true // vacuous .all() over the empty peer set
    }
    fn all_current(&self) -> bool {
        true // vacuous
    }
    fn membership_settled(&self) -> bool {
        self.synced.load(Ordering::SeqCst)
    }
}

#[test]
fn vacuous_gates_cannot_latch_before_membership_syncs() {
    let src = Arc::new(UnsyncedSource { synced: AtomicBool::new(false) });
    let r = Readiness::new(src.clone());
    assert_eq!(
        r.state(),
        ReadinessState::NotReady,
        "empty-but-unsynced membership must not vacuously latch Ready"
    );
    // The informer's initial LIST completes; an (actually) empty cluster —
    // or one whose gates are genuinely satisfied — may now latch.
    src.synced.store(true, Ordering::SeqCst);
    assert_eq!(r.state(), ReadinessState::Ready);
}

#[test]
fn ready_latches_against_blip() {
    let src = FlagSource::new(true, true);
    let r = Readiness::new(src.clone());
    assert_eq!(r.state(), ReadinessState::Ready);
    // A transient peer blip drops current — readiness must NOT revert.
    src.set(true, false);
    assert_eq!(r.state(), ReadinessState::Ready);
    src.set(false, false);
    assert_eq!(r.state(), ReadinessState::Ready);
}

#[test]
fn draining_wins_over_latched_ready() {
    let r = Readiness::new(FlagSource::new(true, true));
    assert_eq!(r.state(), ReadinessState::Ready);
    r.set_draining();
    assert_eq!(r.state(), ReadinessState::Draining);
}

#[test]
fn draining_wins_over_not_ready_and_is_terminal() {
    let src = FlagSource::new(false, false);
    let r = Readiness::new(src.clone());
    assert_eq!(r.state(), ReadinessState::NotReady);
    r.set_draining();
    assert_eq!(r.state(), ReadinessState::Draining);
    // Even if the gate opens afterwards, Draining is terminal.
    src.set(true, true);
    assert_eq!(r.state(), ReadinessState::Draining);
}

#[test]
fn always_ready_is_ready() {
    let r = Readiness::always_ready();
    assert_eq!(r.state(), ReadinessState::Ready);
    r.set_draining();
    assert_eq!(r.state(), ReadinessState::Draining);
}

/// A gating backend's open breaker refuses new calls without a latch,
/// the node still serving; `state` (the `/ready` surface) never follows it.
#[test]
fn an_open_backend_breaker_refuses_new_calls_only_and_never_latches() {
    let r = Readiness::always_ready();
    let (breaker, open) = watch::channel(false);
    r.gate_options_on(open);
    assert_eq!(r.options_report(), OptionsReport::Serving(None));
    breaker.send_replace(true);
    assert_eq!(
        r.options_report(),
        OptionsReport::Serving(Some(Refusal::Backend)),
        "open: serving, refusing new calls"
    );
    assert_eq!(r.state(), ReadinessState::Ready, "/ready unchanged");
    breaker.send_replace(false);
    assert_eq!(r.options_report(), OptionsReport::Serving(None), "closed: back, no latch");
    breaker.send_replace(true);
    r.set_draining();
    assert_eq!(
        r.options_report(),
        OptionsReport::Serving(Some(Refusal::Draining)),
        "draining wins"
    );
}

/// Before the node is ready, the gate changes nothing.
#[test]
fn a_not_ready_node_stays_not_ready_whatever_the_gate() {
    let r = Readiness::new(FlagSource::new(false, false));
    let (breaker, open) = watch::channel(false);
    r.gate_options_on(open);
    assert_eq!(r.options_report(), OptionsReport::NotReady);
    breaker.send_replace(true);
    assert_eq!(r.options_report(), OptionsReport::NotReady);
}

/// Each state's report: NotReady serves nothing, Ready accepts, Draining
/// serves and refuses.
#[test]
fn each_state_reports_its_two_axes() {
    let src = FlagSource::new(false, false);
    let r = Readiness::new(src.clone());
    assert_eq!(r.options_report(), OptionsReport::NotReady);
    src.set(true, true);
    assert_eq!(r.options_report(), OptionsReport::Serving(None));
    r.set_draining();
    assert_eq!(r.options_report(), OptionsReport::Serving(Some(Refusal::Draining)));
    assert_eq!(Refusal::Backend.cause(), "backend");
    assert_eq!(Refusal::Draining.cause(), "draining");
}

/// A cordon refuses new calls and serves every dialog, reversibly: `Ready ⇄
/// Cordoned`, each way idempotent, `/ready` reading the node ready.
#[test]
fn a_cordon_refuses_new_calls_until_it_is_lifted() {
    let r = Readiness::always_ready();
    assert_eq!(r.cordon(), Ok(()));
    assert_eq!(r.cordon(), Ok(()), "idempotent");
    assert_eq!(r.state(), ReadinessState::Cordoned);
    assert!(r.is_cordoned());
    assert_eq!(r.options_report(), OptionsReport::Serving(Some(Refusal::Cordoned)));
    assert_eq!(Refusal::Cordoned.cause(), "cordoned");
    assert_eq!(r.uncordon(), Ok(()));
    assert_eq!(r.uncordon(), Ok(()), "idempotent");
    assert_eq!(r.state(), ReadinessState::Ready);
    assert_eq!(r.options_report(), OptionsReport::Serving(None), "accepting again");
}

/// A cordoned node names the cordon as its cause whatever the backend gate
/// reads, and the backend's cause once the cordon is lifted.
#[test]
fn the_cordon_wins_over_an_open_backend_breaker() {
    let r = Readiness::always_ready();
    let (breaker, open) = watch::channel(true);
    r.gate_options_on(open);
    r.cordon().unwrap();
    assert_eq!(r.options_report(), OptionsReport::Serving(Some(Refusal::Cordoned)));
    r.uncordon().unwrap();
    assert_eq!(r.options_report(), OptionsReport::Serving(Some(Refusal::Backend)));
    breaker.send_replace(false);
    assert_eq!(r.options_report(), OptionsReport::Serving(None));
}

/// No cordon applies to a NotReady node, which has none to lift, nor to a
/// draining one; SIGTERM drains a cordoned node, which keeps its cordon.
#[test]
fn a_cordon_is_refused_while_not_ready_or_draining_and_sigterm_drains_a_cordoned_node() {
    let src = FlagSource::new(false, false);
    let r = Readiness::new(src.clone());
    assert_eq!(r.cordon(), Err(CordonRefused::NotReady));
    assert_eq!(r.uncordon(), Ok(()));
    assert_eq!(r.state(), ReadinessState::NotReady);
    src.set(true, true);
    r.cordon().unwrap();
    r.set_draining();
    assert_eq!(r.state(), ReadinessState::Draining);
    assert_eq!(r.options_report(), OptionsReport::Serving(Some(Refusal::Draining)));
    assert_eq!(r.cordon(), Err(CordonRefused::Draining));
    assert_eq!(r.uncordon(), Err(CordonRefused::Draining));
    assert!(r.is_cordoned(), "the drain keeps the cordon it was given");
    assert_eq!(CordonRefused::NotReady.label(), "not-ready");
    assert_eq!(CordonRefused::Draining.label(), "draining");
}
