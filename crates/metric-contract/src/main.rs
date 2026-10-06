//! `metric-contract` — the metric contract on the command line.
//!
//! - `metric-contract export` prints every shipped binary's catalogue as JSON.
//! - `metric-contract check --catalogue <json> [--allow <file>]... <path>...`
//!   checks every metric name and plain selector label under `<path>` (a file,
//!   or a directory read recursively; `dir/**` reads as `dir`) against the
//!   catalogue, and exits 1 naming each finding. An allow entry no checked
//!   file used is a finding (stale), so give it every path the allow-lists
//!   scope.

use std::path::PathBuf;
use std::process::ExitCode;

use metric_contract::allow::Allow;
use metric_contract::contract::Contract;
use metric_contract::scan::check_paths;

const USAGE: &str = "usage: metric-contract export\n       metric-contract check --catalogue <json> [--allow <file>]... <path>...";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("export") if args.len() == 1 => {
            print!("{}", metric_contract::export());
            ExitCode::SUCCESS
        }
        Some("check") => match check(&args[1..]) {
            Ok(code) => code,
            Err(e) => {
                eprintln!("metric-contract: {e}\n{USAGE}");
                ExitCode::from(2)
            }
        },
        _ => {
            eprintln!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn check(args: &[String]) -> Result<ExitCode, String> {
    let mut catalogue = None;
    let mut allow = Allow::default();
    let mut paths = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--catalogue" => catalogue = Some(it.next().ok_or("--catalogue needs a file")?),
            "--allow" => {
                let file = it.next().ok_or("--allow needs a file")?;
                let text = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
                allow.add(std::path::Path::new(file), &text).map_err(|e| format!("{file}: {e}"))?;
            }
            path => paths.push(PathBuf::from(path)),
        }
    }
    let file = catalogue.ok_or("no --catalogue")?;
    let json = std::fs::read_to_string(file).map_err(|e| format!("{file}: {e}"))?;
    let contract = Contract::from_json(&json)?;
    if paths.is_empty() {
        return Err("no path to check".to_owned());
    }
    let findings = check_paths(&contract, &allow, &paths);
    for f in &findings {
        println!("{f}");
    }
    if findings.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        eprintln!(
            "metric-contract: {} name(s) or label(s) the catalogue does not hold",
            findings.len()
        );
        Ok(ExitCode::FAILURE)
    }
}
