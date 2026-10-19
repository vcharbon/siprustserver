//! The no-panic property lane (ADR-0051 §8): whatever bytes arrive, every
//! entry point of the message layer returns, and a parse failure is a value.
//!
//! Four slow-lane tests (`just test-slow`, which runs this module in the test
//! profile so overflow checks and debug assertions are on), each on threads
//! with a [`LANE_STACK`] stack:
//!
//! - [`structured_corpus_never_panics`]: the deterministic structured
//!   mutations of every seed ([`mutate::families`]) through [`probe`];
//! - [`structured_corpus_parse_results_unchanged`]: the same corpus folded
//!   into one digest per seed and family, compared with `parse_digest.txt`,
//!   so a rewrite of the parser that changes an outcome is seen. A case the
//!   digest file lists as panicking is left out of its digest. After an
//!   intended change, `SIP_MESSAGE_LANE_DIGEST=write` rewrites the file;
//! - [`random_mutations_never_panic`] and [`random_descriptions_never_panic`]:
//!   proptest over stacked random mutations, a fixed case count, failures
//!   persisted under `proptest-regressions/` beside this file.
//!
//! The panics the lane found are the unit tests of [`regressions`], in the
//! default lane.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

use proptest::collection::vec;
use proptest::test_runner::{Config, FileFailurePersistence, RngSeed, TestRunner};

mod echo;
mod mutate;
mod probe;
mod regressions;
mod seeds;

use mutate::Mutation;
use probe::Digest;
use seeds::Seed;

/// The stack the lane runs on: well under a runtime worker's, so a recursion
/// the input drives overflows here first.
const LANE_STACK: usize = 512 * 1024;

/// Cases per proptest property.
const CASES: u32 = 2048;

/// The seed every proptest property draws its cases from.
const LANE_SEED: u64 = 0x5eed_1a5e;

/// Runs `f` on a thread with the lane's stack, its panic re-raised here.
fn on_lane_stack(f: impl FnOnce() + Send + 'static) {
    let worker = std::thread::Builder::new()
        .name("no-panic-lane".into())
        .stack_size(LANE_STACK)
        .spawn(f)
        .expect("spawn the lane thread");
    if let Err(panic) = worker.join() {
        std::panic::resume_unwind(panic);
    }
}

/// One family of one seed, as the corpus ran it.
struct Line {
    seed: String,
    family: &'static str,
    cases: usize,
    digest: u64,
    panicked: Vec<usize>,
}

/// What a case is fed through: a datagram, or a description beside a partner.
enum Target<'a> {
    Message,
    Description(&'a [u8]),
}

fn run_case(target: &Target<'_>, input: &[u8], d: &mut Digest) {
    match target {
        Target::Message => probe::message(input, d),
        Target::Description(partner) => {
            probe::description(input, partner, d);
            probe::description(partner, input, d);
        }
    }
}

/// The lines of one seed's families, `skip` naming the cases left out.
fn seed_lines(
    seed: &Seed,
    target: &Target<'_>,
    skip: &(dyn Fn(&str, &str) -> BTreeSet<usize> + Sync),
) -> Vec<Line> {
    let mut lines = Vec::new();
    for (family, mutations) in mutate::families(&seed.bytes) {
        let skipped = skip(&seed.name, family);
        let mut d = Digest::new();
        let mut panicked = Vec::new();
        for (i, m) in mutations.iter().enumerate() {
            if skipped.contains(&i) {
                continue;
            }
            let input = mutate::apply(&seed.bytes, m);
            let mut case = Digest::new();
            match catch_unwind(AssertUnwindSafe(|| run_case(target, &input, &mut case))) {
                Ok(()) => d.note("case", &case.value()),
                Err(_) => panicked.push(i),
            }
        }
        lines.push(Line {
            seed: seed.name.clone(),
            family,
            cases: mutations.len(),
            digest: d.value(),
            panicked,
        });
    }
    lines
}

/// Threads the corpus is spread over, each on the lane's stack.
const WORKERS: usize = 4;

/// Every seed and family of the corpus, in seed order; `skip` names the cases
/// left out of each line.
fn corpus(skip: &(dyn Fn(&str, &str) -> BTreeSet<usize> + Sync)) -> Vec<Line> {
    let messages = seeds::messages();
    let descriptions = seeds::descriptions();
    let mut jobs: Vec<(&Seed, Target<'_>)> =
        messages.iter().map(|s| (s, Target::Message)).collect();
    for (i, s) in descriptions.iter().enumerate() {
        let partner = &descriptions[(i + 1) % descriptions.len()];
        jobs.push((s, Target::Description(&partner.bytes)));
    }
    let jobs = &jobs;
    let mut done: Vec<(usize, Vec<Line>)> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..WORKERS)
            .map(|w| {
                std::thread::Builder::new()
                    .name(format!("no-panic-lane-{w}"))
                    .stack_size(LANE_STACK)
                    .spawn_scoped(scope, move || {
                        (w..jobs.len())
                            .step_by(WORKERS)
                            .map(|j| (j, seed_lines(jobs[j].0, &jobs[j].1, skip)))
                            .collect::<Vec<_>>()
                    })
                    .expect("spawn a lane worker")
            })
            .collect();
        workers.into_iter().flat_map(|w| w.join().expect("lane worker")).collect()
    });
    done.sort_by_key(|(j, _)| *j);
    done.into_iter().flat_map(|(_, lines)| lines).collect()
}

/// Every structured mutation of every seed returns from every entry point.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_never_panics() {
    let lines = corpus(&|_, _| BTreeSet::new());
    let mut report = String::new();
    for line in &lines {
        for &i in &line.panicked {
            let mutations = mutate::families(&seed_bytes(&line.seed));
            let (_, list) = mutations.iter().find(|(f, _)| *f == line.family).expect("family");
            let _ = writeln!(report, "{} {} #{i}: {:?}", line.seed, line.family, list[i]);
        }
    }
    assert!(report.is_empty(), "entry points panicked on:\n{report}");
}

fn seed_bytes(name: &str) -> Vec<u8> {
    seeds::messages()
        .into_iter()
        .chain(seeds::descriptions())
        .find(|s| s.name == name)
        .map(|s| s.bytes)
        .expect("seed by name")
}

fn digest_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/it/no_panic_lane/parse_digest.txt")
}

/// `seed family -> (cases, digest, panicked)` as the digest file states it.
fn read_digest() -> Vec<(String, String, usize, String, BTreeSet<usize>)> {
    let text = std::fs::read_to_string(digest_path()).unwrap_or_default();
    text.lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .map(|l| {
            let mut f = l.split_whitespace();
            let seed = f.next().expect("seed").to_owned();
            let family = f.next().expect("family").to_owned();
            let cases = f.next().expect("cases").parse().expect("case count");
            let digest = f.next().expect("digest").to_owned();
            let panicked = f
                .next()
                .map(|p| p.split(',').map(|i| i.parse().expect("case index")).collect())
                .unwrap_or_default();
            (seed, family, cases, digest, panicked)
        })
        .collect()
}

fn write_digest(lines: &[Line]) {
    let mut out = String::from(
        "# The no-panic lane's equivalence digest: per seed and mutation family, the\n\
         # case count, the FNV-1a digest of every outcome, and the cases that\n\
         # panicked when it was written (left out of the digest).\n\
         # Rewrite after an intended change: SIP_MESSAGE_LANE_DIGEST=write.\n",
    );
    for l in lines {
        let panicked: Vec<String> = l.panicked.iter().map(usize::to_string).collect();
        let _ = writeln!(
            out,
            "{} {} {} {:016x} {}",
            l.seed,
            l.family,
            l.cases,
            l.digest,
            panicked.join(",")
        );
    }
    std::fs::write(digest_path(), out).expect("write the digest file");
}

/// The corpus's outcomes are those the digest file states.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn structured_corpus_parse_results_unchanged() {
    if std::env::var("SIP_MESSAGE_LANE_DIGEST").is_ok_and(|v| v == "write") {
        write_digest(&corpus(&|_, _| BTreeSet::new()));
        return;
    }
    let stated = read_digest();
    assert!(!stated.is_empty(), "no digest at {}", digest_path().display());
    let skip = |seed: &str, family: &str| {
        stated
            .iter()
            .find(|(s, f, ..)| s == seed && f == family)
            .map(|(.., p)| p.clone())
            .unwrap_or_default()
    };
    let lines = corpus(&skip);
    let mut report = String::new();
    for l in &lines {
        let digest = format!("{:016x}", l.digest);
        let want = stated.iter().find(|(s, f, ..)| *s == l.seed && f == l.family);
        match want {
            Some((_, _, cases, d, _))
                if *cases == l.cases && *d == digest && l.panicked.is_empty() => {}
            Some((_, _, cases, d, _)) => {
                let _ = writeln!(
                    report,
                    "{} {}: {} cases digest {digest} (panicked {:?}), stated {cases} cases digest {d}",
                    l.seed, l.family, l.cases, l.panicked
                );
            }
            None => {
                let _ = writeln!(report, "{} {}: not in the digest file", l.seed, l.family);
            }
        }
    }
    assert!(report.is_empty(), "outcomes changed:\n{report}");
}

/// The lane's runner: a fixed seed, so every run draws the same cases and a
/// red run repeats; a failure is also persisted under `name`.
fn runner(name: &'static str) -> TestRunner {
    TestRunner::new(Config {
        cases: CASES,
        max_shrink_time: 60_000,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(name))),
        source_file: Some(file!()),
        rng_seed: RngSeed::Fixed(LANE_SEED),
        ..Config::default()
    })
}

/// Up to four stacked mutations of any message seed return from every
/// message entry point.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_mutations_never_panic() {
    on_lane_stack(|| {
        let messages: Vec<Vec<u8>> = seeds::messages().into_iter().map(|s| s.bytes).collect();
        let n = messages.len();
        let strategy = (0..n, vec(mutate::mutation(), 1..=4));
        let mut runner = runner("tests/it/no_panic_lane/proptest-regressions/messages.txt");
        let result = runner.run(&strategy, |(seed, mutations)| {
            let input = stack(&messages[seed], &mutations);
            probe::message(&input, &mut Digest::new());
            Ok(())
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}

/// Up to four stacked mutations of two description seeds return from every
/// SDP entry point, each read as either side.
#[test]
#[ignore = "slow lane: no-panic property lane"]
fn random_descriptions_never_panic() {
    on_lane_stack(|| {
        let descriptions: Vec<Vec<u8>> =
            seeds::descriptions().into_iter().map(|s| s.bytes).collect();
        let n = descriptions.len();
        let side = || (0..n, vec(mutate::mutation(), 0..=4));
        let mut runner = runner("tests/it/no_panic_lane/proptest-regressions/descriptions.txt");
        let result = runner.run(&(side(), side()), |((a, ma), (b, mb))| {
            let a = stack(&descriptions[a], &ma);
            let b = stack(&descriptions[b], &mb);
            let mut d = Digest::new();
            probe::description(&a, &b, &mut d);
            probe::description(&b, &a, &mut d);
            Ok(())
        });
        if let Err(e) = result {
            panic!("{e}");
        }
    });
}

fn stack(seed: &[u8], mutations: &[Mutation]) -> Vec<u8> {
    mutations.iter().fold(seed.to_vec(), |acc, m| mutate::apply(&acc, m))
}
