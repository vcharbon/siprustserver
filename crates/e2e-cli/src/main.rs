//! `e2e` — the headless E2E campaign runner (ADR-0018 Phase H). The body
//! lives in the library so the integration tests drive the same surface.

use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ExitCode::from(e2e_cli::cli(&args) as u8)
}
