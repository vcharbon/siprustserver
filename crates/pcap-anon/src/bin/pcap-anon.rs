//! pcap-anon — CLI over the ingest library.
//!
//!   pcap-anon auto <flows.json> <out-dir> --plan <cfg> --profile <cfg> --reading <file>
//!                  --seed-file <path> [--fresh-seed] [--capture <name>] [--verify]
//!   pcap-anon auto <flows.json> <out-dir> --plan <cfg> --profile <cfg> --reading <file>
//!                  --as-is [--capture <name>]
//!     → <out-dir>/<capture>.anon.flows.json, source.txt, auto.json: one
//!       capture's ingest (the `auto` module); `--as-is` takes the document
//!       as it stands, anonymising nothing.
//!
//!   pcap-anon anonymize <flows.json> <out.flows.json> --plan <cfg> --reading <file>
//!                       --seed-file <path> [--fresh-seed] [--profile <cfg>]
//!                       [--all-addresses | --keep-addresses] [--verify]
//!     → a flows document with every number, display name, host name,
//!       charging id, product text and mapped address replaced.
//!
//!   pcap-anon anonymize <flows.json> --check-reproducible [--sample N] [--seed-file <path>] …
//!     → whether two runs over the raw document under one seed agree byte for
//!       byte and the output leaks nothing, and where and in what shape every
//!       token the leak check cannot explain sits.
//!
//! The seed is kept in the file `--seed-file` names (the `seeds` module) and
//! never printed: a capture anonymised again under its kept seed is
//! byte-identical.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use pcap_anon::anonymize::names::NameCounts;
use pcap_anon::fake::address::Scope;
use pcap_anon::reading::Reading;
use pcap_anon::{anonymize, auto, model, plan, profile, reproduce, seeds, verify};

#[derive(Parser)]
#[command(
    name = "pcap-anon",
    about = "SIP capture anonymiser and callflow writer (see the crate's README)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Where the run's seed comes from (the `seeds` module): a file, never the
/// command line, which a process listing shows.
#[derive(Args)]
struct SeedArgs {
    /// The capture's kept seed (the ingest keeps it in the store's seed
    /// directory): read when it exists, drawn and written there (mode 600) by
    /// `--fresh-seed` when it does not.
    #[arg(long)]
    seed_file: Option<PathBuf>,
    /// Draw a new seed and keep it at `--seed-file`; refused where a seed is
    /// kept, since a new seed is a new capture name.
    #[arg(long)]
    fresh_seed: bool,
}

impl SeedArgs {
    fn resolve(&self) -> Result<pcap_anon::fake::Seed, String> {
        seeds::resolve(self.seed_file.as_deref(), self.fresh_seed)
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Rewrite every identity of a raw flows document to a fake, or check that
    /// two runs under one seed agree and leak nothing.
    Anonymize {
        /// Source flows.json.
        flows: PathBuf,
        /// Destination flows.json (overwritten if present); not with
        /// `--check-reproducible`.
        out: Option<PathBuf>,
        /// The source network's numbering-plan equivalence config (JSON).
        #[arg(long)]
        plan: PathBuf,
        /// The reading the capture was extracted under (`sipflow --reading`).
        #[arg(long)]
        reading: PathBuf,
        /// Keep the SUT addresses and texts of a network profile
        /// (`*.profile.json`) and read its body types; under
        /// `--all-addresses`, read its body types alone. Without one, a body
        /// part of a type no RFC defines is redacted whole.
        #[arg(long)]
        profile: Option<PathBuf>,
        /// Map the private address ranges too: a document that leaves the
        /// network (a committed fixture) keeps no address and no kept text of
        /// it.
        #[arg(long)]
        all_addresses: bool,
        /// Map no address: every peer, public or private, stays as captured
        /// (a document for this network's own tooling only).
        #[arg(long, conflicts_with = "all_addresses")]
        keep_addresses: bool,
        #[command(flatten)]
        seed: SeedArgs,
        /// Search every number-bearing user-part and every mapped name of the
        /// input in the output, every display name of the output for a letter
        /// left unmasked, and the output for any domain, address, free-text
        /// word, non-ASCII run or digit run no fake of the run explains, and
        /// fail, writing nothing, on any (counts are reported, never a value).
        #[arg(long)]
        verify: bool,
        /// Anonymise the raw input twice under one seed (the kept one with
        /// `--seed-file`, else one kept nowhere) and report how the outputs
        /// differ and every token the leak check cannot explain, by its site
        /// and shape; exits 1 unless the outputs agree byte for byte and
        /// nothing leaks. Refuses an anonymised input. Writes nothing.
        #[arg(long, conflicts_with_all = ["out", "verify"])]
        check_reproducible: bool,
        /// With `--check-reproducible`, read only the first N legs.
        #[arg(long, requires = "check_reproducible")]
        sample: Option<usize>,
    },
    /// One capture's ingest: the anonymised flows document, `source.txt` and
    /// an `auto.json` summary of the anonymisation. A capture that does not
    /// land is summary data, not a process failure.
    Auto {
        /// flows.json produced by `sipflow <pcap> --json`.
        flows: PathBuf,
        /// Output directory (created if missing).
        out: PathBuf,
        /// The source network's numbering-plan equivalence config (JSON).
        #[arg(long)]
        plan: PathBuf,
        /// The network profile (`*.profile.json`) naming the SUT addresses
        /// and texts the anonymiser keeps.
        #[arg(long)]
        profile: PathBuf,
        /// The reading the capture was extracted under (`sipflow --reading`).
        #[arg(long)]
        reading: PathBuf,
        /// Capture name; defaults to the flows file stem (`.flows`/`.anon`
        /// suffixes stripped).
        #[arg(long)]
        capture: Option<String>,
        #[command(flatten)]
        seed: SeedArgs,
        /// Search every number-bearing user-part and every mapped name of the
        /// input in the anonymised document, every display name of it for a
        /// letter left unmasked, and the document for any domain, address,
        /// free-text word, non-ASCII run or digit run no fake of the run
        /// explains, and fail, persisting only `auto.json`, on any.
        #[arg(long)]
        verify: bool,
        /// Take the document as it stands: no anonymisation, no seed, no
        /// verification (one the inbox anonymised from the raw read, or a
        /// lab capture kept unanonymised by choice).
        #[arg(long, conflicts_with_all = ["seed_file", "fresh_seed", "verify"])]
        as_is: bool,
    },
}

fn main() -> ExitCode {
    match run() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("pcap-anon: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ExitCode, String> {
    match Cli::parse().cmd {
        Cmd::Anonymize {
            flows,
            out,
            plan,
            reading,
            profile,
            all_addresses,
            keep_addresses,
            seed,
            verify,
            check_reproducible,
            sample,
        } => {
            let plan = plan::Plan::from_json_str(&auto::read_file(&plan, "plan")?)?;
            let reading = Reading::stated(
                &auto::read_file(&reading, "reading")?,
                &reading.display().to_string(),
            )?;
            let profile = match profile {
                Some(p) => Some(profile::Profile::from_json_str(&auto::read_file(&p, "profile")?)?),
                None => None,
            };
            let mut scope = match &profile {
                Some(profile) if all_addresses => {
                    Scope::default().all_addresses().reading_bodies(&profile.bodies)
                }
                Some(profile) => profile.scope(),
                None if all_addresses => Scope::default().all_addresses(),
                None => Scope::default(),
            };
            if keep_addresses {
                scope = scope.keep_addresses();
            }
            if check_reproducible {
                return check_reproducible_cmd(flows, &plan, &scope, &reading, &seed, sample);
            }
            let out = out.ok_or("anonymize needs an output path (or --check-reproducible)")?;
            let rules = Rules { plan: &plan, scope: &scope, reading: &reading };
            anonymize_cmd(flows, out, &rules, &seed, verify).map(|()| ExitCode::SUCCESS)
        }
        Cmd::Auto { flows, out, plan, profile, reading, capture, seed, verify, as_is } => {
            let config = auto::Config::read(&plan, &profile, &reading)?;
            auto_cmd(flows, out, &config, capture, &seed, verify, as_is).map(|()| ExitCode::SUCCESS)
        }
    }
}

/// What one `anonymize` runs under.
struct Rules<'a> {
    plan: &'a plan::Plan,
    scope: &'a Scope,
    reading: &'a Reading,
}

fn read(path: &std::path::Path) -> Result<String, String> {
    auto::read_file(path, "flows")
}

fn auto_cmd(
    flows: PathBuf,
    out: PathBuf,
    config: &auto::Config,
    capture: Option<String>,
    seed: &SeedArgs,
    verify: bool,
    as_is: bool,
) -> Result<(), String> {
    let capture = capture.unwrap_or_else(|| default_capture_name(&flows));
    let summary = if as_is {
        auto::run_as_is(&read(&flows)?, &capture, &out, &config.reading)?
    } else {
        auto::run(&read(&flows)?, &capture, &out, config, &seed.resolve()?, verify)?
    };
    auto::write_summary(&summary, &out)?;
    if let Some(a) = &summary.anonymized {
        println!(
            "{capture}: {} identities rewritten ({} unrecognised, {} keys share a fake, {} fakes \
             nest in a longer one, {} Call-IDs merged, {} UUI skipped, {} UUI redacted); {} hosts, {} charging ids, \
             {} texts, {} addresses ({} share a fake), {} values and {} body parts redacted{}",
            a.identities,
            a.unrecognised,
            a.shared_keys,
            a.nested_fakes,
            a.merged_call_ids,
            a.uui_skipped,
            a.uui_redacted,
            a.hosts,
            a.charging_ids,
            a.texts,
            a.addresses,
            a.name_collisions,
            a.redacted,
            a.redacted_parts,
            verified_text(a.verified)
        );
    }
    if let Some(e) = &summary.error {
        if summary.anonymized.as_ref().is_some_and(|a| a.verified.is_some()) {
            return Err(format!("{capture}: {e}; only auto.json was written"));
        }
        println!("{capture}: NOT LANDED ({e})");
        return Ok(());
    }
    println!("{capture}: landed — see auto.json");
    Ok(())
}

fn anonymize_cmd(
    flows: PathBuf,
    out: PathBuf,
    rules: &Rules<'_>,
    seed: &SeedArgs,
    verify: bool,
) -> Result<(), String> {
    let Rules { plan, scope, reading } = *rules;
    let seed = seed.resolve()?;
    let json = read(&flows)?;
    let mut model = model::ingest_str(&json).map_err(|e| e.to_string())?;
    let report =
        anonymize::anonymize_in(&mut model, plan, &seed, scope).map_err(|e| e.to_string())?;
    let verdict = verify
        .then(|| {
            let raw = model::ingest_str(&json).map_err(|e| e.to_string())?;
            Ok::<_, String>(verify::verdict::judge(&raw, &model, plan, scope, reading, &report))
        })
        .transpose()?;
    if let Some(v) = verdict.filter(|v| v.total() > 0) {
        return Err(format!("{}; {} not written", v.refusal(), out.display()));
    }
    std::fs::write(&out, model::to_json_string(&model, reading)?)
        .map_err(|e| format!("cannot write {}: {e}", out.display()))?;
    let names = NameCounts::of(&report.names);
    println!(
        "wrote {} ({} identities rewritten, {} unrecognised, {} keys share a fake, {} fakes nest \
         in a longer one, {} Call-IDs merged, {} UUI skipped, {} UUI redacted; {} hosts, {} charging ids, {} texts, \
         {} addresses, {} sharing a fake, {} values and {} body parts redacted{})",
        out.display(),
        report.entries.len(),
        report.unrecognised(),
        report.shared_keys(),
        report.nested_fakes(),
        report.merged_call_ids,
        report.uui_skipped,
        report.uui_redacted,
        names.hosts,
        names.icids,
        names.texts,
        names.addresses,
        report.name_collisions,
        names.redacted,
        report.redacted_parts,
        verified_text(verdict)
    );
    Ok(())
}

/// The check reads the kept seed when one is named; otherwise it draws a
/// seed it keeps nowhere, since it writes nothing.
fn check_reproducible_cmd(
    flows: PathBuf,
    plan: &plan::Plan,
    scope: &Scope,
    reading: &Reading,
    seed: &SeedArgs,
    sample: Option<usize>,
) -> Result<ExitCode, String> {
    if seed.fresh_seed {
        return Err("--check-reproducible writes nothing: no --fresh-seed".to_string());
    }
    let seed = match &seed.seed_file {
        Some(_) => seed.resolve()?,
        None => pcap_anon::fake::Seed::random()?,
    };
    let model = model::ingest_str(&read(&flows)?).map_err(|e| e.to_string())?;
    let report =
        reproduce::check(model, plan, scope, reading, &seed, sample).map_err(|e| e.to_string())?;
    println!(
        "reproducible: {} legs, {} datagrams read; two runs under one seed differ in {} \
         datagrams{}; {} survivors, {} unmasked display names, {} surviving names, {} merged \
         identifiers, {} tokens no fake explains",
        report.legs,
        report.datagrams,
        report.differing,
        if report.documents_differ { " (the documents differ)" } else { "" },
        report.verdict.survivors,
        report.verdict.unmasked_displays,
        report.verdict.name_survivors,
        report.verdict.merged_identifiers,
        report.findings.len()
    );
    for f in &report.findings {
        println!("  {:<14} {:<24} {:<32} {} datagram(s)", f.class, f.site, f.shape, f.datagrams);
    }
    Ok(if report.holds() { ExitCode::SUCCESS } else { ExitCode::FAILURE })
}

/// The `--verify` verdict as the report line states it; empty when the check
/// did not run.
fn verified_text(verdict: Option<verify::verdict::Verdict>) -> String {
    verdict.map_or(String::new(), |v| {
        format!(
            ", verified: {} survivors, {} unmasked display names, {} surviving names, {} merged \
             identifiers, {} tokens no fake explains",
            v.survivors,
            v.unmasked_displays,
            v.name_survivors,
            v.merged_identifiers,
            v.leaks.total()
        )
    })
}

/// The capture name a flows file encodes: its stem with the pipeline's
/// `.flows` and `.anon` suffixes stripped (`call1.flows.json` → `call1`;
/// `call1.anon.flows.json` → `call1`).
fn default_capture_name(flows: &std::path::Path) -> String {
    let stem = flows.file_stem().and_then(|s| s.to_str()).unwrap_or("capture");
    let stem = stem.strip_suffix(".flows").unwrap_or(stem);
    stem.strip_suffix(".anon").unwrap_or(stem).to_string()
}
