//! `pivot-replay <run-spec.json>`: the replay with nothing registered. It runs
//! one case on the simulated lane against the plain system
//! (`pivot_replay::plain`), leaves the run bundle and exits with the
//! structural verdict. `pivot-replay batch <batch-spec.json>` is the batch
//! process of a hosted lane against the plain hosted system
//! (`pivot_replay::plain_batch`). `pivot-replay http-stub <spec.json>` serves an
//! environment's stub HTTP service (`pivot_replay::http_stub`);
//! `pivot-replay schema` prints the run-spec JSON Schema.
//!
//! Exit codes: 0 pass, 1 failed run (verdict, post-run cleanup or RFC audit), 2 refused
//! (lint / compile / spec), 3 environment, 4 panicked run.

use pivot_replay::spec::RunSpec;
use pivot_replay::{execute, exit_code, summary_line};

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(arg) = args.next() else {
        eprintln!(
            "usage: pivot-replay <run-spec.json> | pivot-replay batch <batch-spec.json> | \
             pivot-replay http-stub <spec.json> | pivot-replay schema"
        );
        std::process::exit(3);
    };
    if arg == "batch" {
        let Some(spec) = args.next() else {
            eprintln!("usage: pivot-replay batch <batch-spec.json>");
            std::process::exit(3);
        };
        std::process::exit(pivot_replay::batch::run::<pivot_replay::plain_batch::PlainBatch>(
            std::path::Path::new(&spec),
        ));
    }
    if arg == "http-stub" {
        let Some(spec) = args.next() else {
            eprintln!("usage: pivot-replay http-stub <spec.json>");
            std::process::exit(3);
        };
        std::process::exit(pivot_replay::http_stub::run(std::path::Path::new(&spec)));
    }
    if arg == "schema" {
        let schema = schemars::schema_for!(RunSpec);
        println!("{}", serde_json::to_string_pretty(&schema).expect("schema serializes"));
        return;
    }
    let result = RunSpec::read(std::path::Path::new(&arg)).and_then(|spec| {
        let result = execute(&spec);
        if let Ok(summary) = &result {
            println!("{}", summary_line(&spec.case.display().to_string(), summary));
        }
        result
    });
    if let Err(failure) = &result {
        eprintln!("{failure}");
    }
    std::process::exit(exit_code(&result));
}
