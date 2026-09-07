//! `pivot-schema` — publish the pivot contracts, normalise documents to them and
//! check what a schema cannot.
//!
//! Five subcommands, one product each: `schema` emits the JSON Schema other
//! languages' mirrors are checked against — for the two authored documents and
//! for the four record kinds a run leaves behind — `fmt` is the normative
//! canonical formatter, `lint` runs the semantic rules, `tiers` emits the
//! tier / body-handling data a generator would otherwise reverse-engineer, and
//! `schedules` the retransmission ladders it would otherwise walk itself.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};
use pivot_schema::bundle::{RecordedMessage, RunConfig, RunRfcAudit, RunTiming, RunVerdict};
use pivot_schema::{PivotV3, RuleFile, canonical, lint, schedules, tiers};

#[derive(Parser)]
#[command(name = "pivot-schema", about = "Pivot v3 schemas, canonical formatter, lint, tier data and retransmission schedules")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print a JSON Schema on stdout.
    Schema {
        #[arg(value_enum)]
        which: Contract,
    },
    /// Parse a document and re-print it canonically on stdout. Non-zero on a
    /// parse error, so a pipeline cannot format its way past a broken file.
    Fmt {
        file: PathBuf,
        /// Which contract to parse the file as. Inferred from the document
        /// itself when omitted.
        #[arg(long, value_enum)]
        as_contract: Option<Contract>,
        /// Rewrite the file in place instead of printing it.
        #[arg(long)]
        write: bool,
        /// Print nothing and exit non-zero when the file is not already
        /// canonical.
        #[arg(long, conflicts_with = "write")]
        check: bool,
    },
    /// Run the semantic rules over a pivot. Non-zero on any error-severity
    /// finding, so a pipeline cannot replay a document that means nothing.
    Lint {
        file: PathBuf,
        /// Emit the report as JSON rather than as text.
        #[arg(long)]
        json: bool,
    },
    /// Print the normative tier and body-handling data on stdout.
    Tiers,
    /// Print the retransmission schedule every ladder rides on stdout: per
    /// class, the rung intervals to the give-up, in milliseconds.
    Schedules,
}

/// The contracts this crate owns: the two documents a pipeline authors, and the
/// four record kinds one run leaves behind.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Contract {
    /// The pivot v3 scenario document.
    Pivot,
    /// The correlation-rule file.
    Rules,
    /// A run bundle's `run-config.json`: the lane's compiled configuration.
    RunConfig,
    /// A run bundle's `verdict.json`: what the run decided, and why.
    Verdict,
    /// A run bundle's `timing.json`: when the run started and settled.
    Timing,
    /// One line of a run bundle's `recording/<leg>.jsonl`.
    Recording,
    /// A run bundle's `rfc.json`: the post-run RFC audit, or that none ran.
    Rfc,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("pivot-schema: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Schema { which } => {
            let schema = match which {
                Contract::Pivot => schemars::schema_for!(PivotV3),
                Contract::Rules => schemars::schema_for!(RuleFile),
                Contract::RunConfig => schemars::schema_for!(RunConfig),
                Contract::Verdict => schemars::schema_for!(RunVerdict),
                Contract::Timing => schemars::schema_for!(RunTiming),
                Contract::Recording => schemars::schema_for!(RecordedMessage),
                Contract::Rfc => schemars::schema_for!(RunRfcAudit),
            };
            print!("{}", canonical::format(&schema).map_err(|e| e.to_string())?);
            Ok(())
        }
        Command::Fmt { file, as_contract, write, check } => {
            let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let formatted = format_as(&text, as_contract).map_err(|e| format!("{}: {e}", file.display()))?;
            if check {
                return if formatted == text {
                    Ok(())
                } else {
                    Err(format!("{}: not canonically formatted", file.display()))
                };
            }
            if write {
                std::fs::write(&file, &formatted).map_err(|e| format!("{}: {e}", file.display()))?;
            } else {
                print!("{formatted}");
            }
            Ok(())
        }
        Command::Lint { file, json } => {
            let text = std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
            let report = lint::lint_str(&text);
            if json {
                println!("{}", canonical::format(&report).map_err(|e| e.to_string())?);
            } else {
                print!("{}", report.render());
            }
            if report.has_errors() {
                return Err(format!("{}: lint failed", file.display()));
            }
            Ok(())
        }
        Command::Tiers => {
            print!("{}", canonical::format(&tiers::tier_data()).map_err(|e| e.to_string())?);
            Ok(())
        }
        Command::Schedules => {
            print!("{}", canonical::format(&schedules::schedule_table()).map_err(|e| e.to_string())?);
            Ok(())
        }
    }
}

/// Parse through the typed model — so `fmt` proves the document conforms, not
/// merely that it is JSON — then re-serialize canonically.
fn format_as(text: &str, contract: Option<Contract>) -> Result<String, String> {
    let contract = match contract {
        Some(c) => c,
        None => infer(text)?,
    };
    match contract {
        Contract::Pivot => {
            let pivot = PivotV3::from_json(text).map_err(|e| e.to_string())?;
            if !pivot.version_matches() {
                return Err(format!(
                    "pivot_version {} — this tool owns version {}",
                    pivot.pivot_version,
                    pivot_schema::PIVOT_VERSION
                ));
            }
            canonical::format(&pivot).map_err(|e| e.to_string())
        }
        Contract::Rules => {
            let rules = RuleFile::from_json(text).map_err(|e| e.to_string())?;
            match rules.violations().as_slice() {
                [] => canonical::format(&rules).map_err(|e| e.to_string()),
                found => Err(found.join("; ")),
            }
        }
        Contract::RunConfig => reformat::<RunConfig>(text),
        Contract::Verdict => reformat::<RunVerdict>(text),
        Contract::Timing => reformat::<RunTiming>(text),
        Contract::Rfc => reformat::<RunRfcAudit>(text),
        // A recording is a stream, not a document: `fmt` formats one file as one
        // value, and reformatting a line at a time would be a different tool.
        Contract::Recording => Err(
            "a recording is JSON Lines — one message per line, not one document; \
             `schema recording` publishes the line's contract"
                .into(),
        ),
    }
}

/// Parse through the typed model, then re-serialize canonically — the same
/// proof `fmt` gives a pivot, for the record kinds a run writes.
fn reformat<T: serde::de::DeserializeOwned + serde::Serialize>(text: &str) -> Result<String, String> {
    let value: T = serde_json::from_str(text).map_err(|e| e.to_string())?;
    canonical::format(&value).map_err(|e| e.to_string())
}

/// Each contract declares a field the others do not: a pivot `pivot_version`, a
/// rule file `rules`, a verdict `status`, a run configuration `route_target`, a
/// timing `settle_budget_ms`, a recording line `at_us`. An audit shares the
/// verdict's `status` key and is never inferred: name it with `--as-contract`.
/// Nothing else is guessed.
fn infer(text: &str) -> Result<Contract, String> {
    let value: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let object = value.as_object().ok_or("not a JSON object")?;
    for (key, contract) in [
        ("pivot_version", Contract::Pivot),
        ("rules", Contract::Rules),
        ("status", Contract::Verdict),
        ("route_target", Contract::RunConfig),
        ("settle_budget_ms", Contract::Timing),
        ("at_us", Contract::Recording),
    ] {
        if object.contains_key(key) {
            return Ok(contract);
        }
    }
    Err("no contract of this crate: none of `pivot_version`, `rules`, `status`, \
         `route_target`, `settle_budget_ms` or `at_us` is present"
        .into())
}
