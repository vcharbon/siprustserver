//! The no-panic property lane of the proxy's per-datagram path (ADR-0051
//! §8): whatever datagram a caller, a worker or a callee sends, the recv loop
//! every call shares keeps turning — its Via, cookie and branch decoding and
//! its stateless forward decision included — and the parser it calls never
//! panics.
//!
//! A case ([`rig::case`]) runs one proxy core over the simulated fabric with
//! the load balancer the runner wires, on a paused clock: the seed's
//! unmutated preamble, the mutated datagram twice from the seed's peer, the
//! seed's follow-up, and the clock past the proxy's sweeps. A case fails
//! when the recv loop has ended or the parse guard counted a panic. The
//! seeds ([`seeds::all`]) are recorded off a run of the same proxy, so they
//! carry its own Via branch, cookie and Record-Route. A case runs on one of
//! the rig's variants ([`rig::Variant`]): its self-gate closed, no worker
//! alive, one worker down, two faces, names that resolve to nothing. The
//! health probe reading a worker's answer ([`probe::case`]) and the load
//! report's value codec ([`report::case`]) run beside it.
//!
//! - [`every_seed_survives_every_variant`] (default lane): every seed,
//!   unmutated, on every variant;
//! - [`every_variant_reaches_its_path`] (default lane): each variant reaches
//!   the path it exists for — the proxy's own refusal, both faces, the
//!   other owner, a negative entry;
//! - [`a_probe_answer_feeds_the_observer`] (default lane): each seed answer
//!   of the probe is read and routed on;
//!
//! Two slow-lane tests (`just test-slow` runs them in the test profile, so
//! overflow checks and debug assertions are on), on threads with the lane's
//! small stack:
//!
//! - [`structured_corpus_never_panics`]: every deterministic structured
//!   mutation of every seed ([`wire_mutations::mutate::families`]);
//! - [`random_mutations_never_panic`]: proptest over stacked random
//!   mutations on any variant, a fixed case count, failures persisted under
//!   `proptest-regressions/` beside this file;
//! - [`random_probe_answers_never_panic`]: proptest over stacked random
//!   mutations of the probe's seed answers;
//! - [`random_reports_never_panic`]: proptest over load reports of any text.

use std::fmt::Write as _;

use proptest::collection::vec;
use proptest::strategy::Strategy as _;
use wire_mutations::lane::{on_lane_stack, runner, LANE_STACK};
use wire_mutations::{mutate, Mutation};

mod probe;
mod report;
mod rig;
mod seeds;

use std::sync::Arc;

use rig::{Peer, Variant};
use sip_proxy::load_observer::EluBand;
use sip_proxy::ProxyMetrics;

/// Cases of the proptest property.
const CASES: u32 = 1024;

/// Threads the corpus is spread over, each on the lane's stack.
const WORKERS: usize = 4;

/// Every seed, unmutated, is forwarded: the lane drives the proxy's routing
/// and relay paths, not only its parse refusals.
#[test]
fn every_seed_is_forwarded() {
    on_lane_stack(|| {
        for seed in seeds::all() {
            let outcome = rig::case(&seed.before, seed.from, &seed.bytes, &seed.after);
            assert_eq!(outcome.failure, None, "{}", seed.name);
            assert!(outcome.forwarded > 0, "{} was forwarded nowhere: {outcome:?}", seed.name);
        }
    });
}

/// Every seed of every variant, unmutated, leaves the recv loop turning.
#[test]
fn every_seed_survives_every_variant() {
    on_lane_stack(|| {
        for variant in Variant::DATAGRAM {
            for seed in seeds::all_on(variant) {
                let outcome =
                    rig::case_on(variant, &seed.before, seed.from, &seed.bytes, &seed.after);
                assert_eq!(outcome.failure, None, "{variant:?} {}", seed.name);
            }
        }
    });
}

/// What the rig of `variant` did with `datagrams`, sent in order: each
/// datagram a peer received, and the proxy's metrics.
fn reach(
    variant: Variant,
    datagrams: &[(Peer, Vec<u8>)],
) -> (Vec<(Peer, Vec<u8>)>, Arc<ProxyMetrics>) {
    rig::runtime().block_on(async {
        let rig = rig::Rig::with(variant).await;
        let mut received = Vec::new();
        for (from, datagram) in datagrams {
            rig.send(*from, datagram).await;
            for peer in [Peer::Caller, Peer::WorkerA, Peer::WorkerB, Peer::Callee] {
                received.extend(rig.drain(peer).into_iter().map(|d| (peer, d)));
            }
        }
        (received, rig.metrics.clone())
    })
}

fn seed(variant: Variant, name: &str) -> seeds::Seed {
    let found = seeds::all_on(variant).into_iter().find(|s| s.name == name);
    found.unwrap_or_else(|| panic!("seed {name}"))
}

/// Each variant reaches the path it exists for: the proxy answers the
/// caller's INVITE itself where its self-gate is closed or no worker is
/// alive; a dual-face proxy record-routes the INVITE on both faces; a
/// callee's 200 for a worker that is down reaches the call's other owner;
/// a name that resolves to nothing is held as a negative entry.
#[test]
fn every_variant_reaches_its_path() {
    on_lane_stack(|| {
        let invite = vec![(Peer::Caller, seeds::caller_invite())];
        for variant in [Variant::GateClosed, Variant::NoWorker] {
            let (received, _) = reach(variant, &invite);
            let refused =
                received.iter().any(|(p, d)| *p == Peer::Caller && d.starts_with(b"SIP/2.0 503"));
            let forwarded =
                received.iter().any(|(p, _)| matches!(p, Peer::WorkerA | Peer::WorkerB));
            assert!(refused && !forwarded, "{variant:?}: {received:?}");
        }

        let (received, _) = reach(Variant::DualFace, &invite);
        let at_worker = received.iter().find(|(p, _)| matches!(p, Peer::WorkerA | Peer::WorkerB));
        let at_worker = String::from_utf8_lossy(&at_worker.expect("the INVITE reaches a worker").1)
            .into_owned();
        let routes: Vec<&str> =
            at_worker.lines().filter(|l| l.starts_with("Record-Route:")).collect();
        let faces = [rig::PROXY, rig::PROXY_INTERNAL].map(|(host, port)| format!("{host}:{port}"));
        assert!(faces.iter().all(|f| routes.iter().any(|r| r.contains(f.as_str()))), "{routes:?}");

        let down = Variant::WorkerDown(Peer::WorkerA);
        let answered = seed(down, "resp/200-callee");
        let sent: Vec<(Peer, Vec<u8>)> =
            answered.before.into_iter().chain([(answered.from, answered.bytes)]).collect();
        let (received, _) = reach(down, &sent);
        let to_backup =
            received.iter().any(|(p, d)| *p == Peer::WorkerB && d.starts_with(b"SIP/2.0 200"));
        assert!(to_backup, "the 200 reaches the other owner: {received:?}");

        for (variant, name, outcome) in [
            (Variant::Plain, "req/worker-invite-named", "resolved"),
            (Variant::ResolverGaps, "req/worker-invite-named", "resolved"),
            (Variant::ResolverGaps, "req/worker-invite-unnamed", "dropped_negative"),
        ] {
            let named = seed(variant, name);
            let twice = vec![(named.from, named.bytes.clone()), (named.from, named.bytes)];
            let (_, metrics) = reach(variant, &twice);
            assert!(metrics.named_send_count(outcome) > 0, "{variant:?} {name}: {outcome}");
        }
    });
}

/// Each seed answer of the probe leaves the probe and the recv loop turning,
/// and worker a's loaded report puts it above the critical band.
#[test]
fn a_probe_answer_feeds_the_observer() {
    on_lane_stack(|| {
        for lines in probe::seed_lines() {
            for status in probe::STATUSES {
                let probed = probe::case(status, lines);
                let text = String::from_utf8_lossy(lines);
                assert_eq!(probed.outcome.failure, None, "{status} {text}");
                assert!(probed.outcome.forwarded > 0, "{status} {text}");
                let loaded = text.contains("elu=0.97");
                let critical = probed.band == Some(EluBand::AboveCritical);
                assert_eq!(critical, loaded, "{status} {text}: {:?}", probed.band);
            }
        }
    });
}

/// Every structured mutation of every seed leaves the recv loop turning.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    let seeds = seeds::all();
    let jobs: Vec<(usize, &'static str, Mutation)> = seeds
        .iter()
        .enumerate()
        .flat_map(|(i, s)| {
            mutate::families(&s.bytes)
                .into_iter()
                .flat_map(move |(family, ms)| ms.into_iter().map(move |m| (i, family, m)))
        })
        .collect();
    let (seeds, jobs) = (&seeds, &jobs);
    let failed: Vec<String> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|w| {
                std::thread::Builder::new()
                    .name(format!("no-panic-lane-{w}"))
                    .stack_size(LANE_STACK)
                    .spawn_scoped(scope, move || {
                        let mut failed = Vec::new();
                        for (i, family, m) in jobs.iter().skip(w).step_by(WORKERS) {
                            let seed = &seeds[*i];
                            let input = mutate::apply(&seed.bytes, m);
                            let outcome = rig::case(&seed.before, seed.from, &input, &seed.after);
                            if let Some(f) = outcome.failure {
                                failed.push(format!("{} {family} {m:?}: {f:?}", seed.name));
                            }
                        }
                        failed
                    })
                    .expect("spawn a lane worker")
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("lane worker")).collect()
    });
    let mut report = String::new();
    for line in &failed {
        let _ = writeln!(report, "{line}");
    }
    assert!(report.is_empty(), "the recv loop did not survive:\n{report}");
}

/// Up to four stacked mutations of any seed on any variant leave the recv
/// loop turning.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_mutations_never_panic() {
    on_lane_stack(|| {
        let variants = Variant::DATAGRAM;
        let seeds: Vec<Vec<seeds::Seed>> = variants.iter().map(|v| seeds::all_on(*v)).collect();
        let strategy = (0..variants.len(), 0..seeds[0].len(), vec(mutate::mutation(), 1..=4));
        let mut runner = runner("tests/it/no_panic_lane/proptest-regressions/datagrams.txt", CASES);
        let result = runner.run(&strategy, |(v, i, mutations)| {
            let seed = &seeds[v][i];
            let input = mutations.iter().fold(seed.bytes.clone(), |acc, m| mutate::apply(&acc, m));
            match rig::case_on(variants[v], &seed.before, seed.from, &input, &seed.after).failure {
                None => Ok(()),
                Some(f) => Err(proptest::test_runner::TestCaseError::fail(format!("{f:?}"))),
            }
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}

/// Up to four stacked mutations of a seed answer of the probe, under any
/// status, leave the probe and the recv loop turning.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_probe_answers_never_panic() {
    on_lane_stack(|| {
        let lines = probe::seed_lines();
        let strategy = (0..probe::STATUSES.len(), 0..lines.len(), vec(mutate::mutation(), 1..=4));
        let mut runner = runner("tests/it/no_panic_lane/proptest-regressions/probe.txt", CASES / 4);
        let result = runner.run(&strategy, |(s, i, mutations)| {
            let input = mutations.iter().fold(lines[i].to_vec(), |acc, m| mutate::apply(&acc, m));
            match probe::case(probe::STATUSES[s], &input).outcome.failure {
                None => Ok(()),
                Some(f) => Err(proptest::test_runner::TestCaseError::fail(format!("{f:?}"))),
            }
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}

/// A load report of any text is read without a panic, and every payload that
/// reads is in range.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_reports_never_panic() {
    let line =
        "v=1; elu=[-+0-9.eE]{0,8}; gc=[-+0-9.eEnaif]{0,8}; adm=[-+0-9.eEnaif]{0,12}|[ -~]{0,40}";
    let strategy = vec(line, 1..6).prop_map(|lines| lines.join("\n"));
    let mut runner = runner("tests/it/no_panic_lane/proptest-regressions/reports.txt", CASES * 4);
    let result = runner.run(&strategy, |report| {
        report::case(report.as_bytes());
        Ok(())
    });
    if let Err(e) = result {
        panic!("{e}");
    }
}
