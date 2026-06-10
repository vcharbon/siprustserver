//! The `e2e` CLI body (ADR-0018 Phase H). `cli(args) -> exit code` is a
//! library function so the integration tests drive the real command surface
//! without spawning the binary.

use std::path::{Path, PathBuf};

use e2e_core::model;
use e2e_core::run::{self, CampaignSpec};

const USAGE: &str = "usage:
  e2e run <campaign.json> [--case <id>]... [--infra <id>]...
          [--e2e-dir <dir>] [--runs-root <dir>] [--ts <label>]
      Run a campaign; exit 0 only if EVERY cell passed.
  e2e validate <file.json>...
      Lint authored docs (test case / check set / campaign / endpoint config);
      test cases are additionally validated against the compiled registries.
  e2e schema [--out <dir>]
      Regenerate the JSON Schemas (default out: <cwd>/e2e/schemas).";

pub fn cli(args: &[String]) -> i32 {
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("validate") => cmd_validate(&args[1..]),
        Some("schema") => cmd_schema(&args[1..]),
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

fn fail_usage(msg: &str) -> i32 {
    eprintln!("e2e: {msg}\n\n{USAGE}");
    2
}

// ---------------------------------------------------------------------------
// e2e run
// ---------------------------------------------------------------------------

fn cmd_run(args: &[String]) -> i32 {
    let mut campaign_path: Option<PathBuf> = None;
    let mut cases: Vec<String> = Vec::new();
    let mut infras: Vec<String> = Vec::new();
    let mut e2e_dir: Option<PathBuf> = None;
    let mut runs_root: Option<PathBuf> = None;
    let mut ts: Option<String> = None;

    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut flag_value = |name: &str| -> Result<String, i32> {
            it.next().cloned().ok_or_else(|| fail_usage(&format!("{name} needs a value")))
        };
        match a.as_str() {
            "--case" => match flag_value("--case") {
                Ok(v) => cases.push(v),
                Err(c) => return c,
            },
            "--infra" => match flag_value("--infra") {
                Ok(v) => infras.push(v),
                Err(c) => return c,
            },
            "--e2e-dir" => match flag_value("--e2e-dir") {
                Ok(v) => e2e_dir = Some(PathBuf::from(v)),
                Err(c) => return c,
            },
            "--runs-root" => match flag_value("--runs-root") {
                Ok(v) => runs_root = Some(PathBuf::from(v)),
                Err(c) => return c,
            },
            "--ts" => match flag_value("--ts") {
                Ok(v) => ts = Some(v),
                Err(c) => return c,
            },
            other if other.starts_with("--") => {
                return fail_usage(&format!("unknown flag {other:?}"));
            }
            other if campaign_path.is_none() => campaign_path = Some(PathBuf::from(other)),
            other => return fail_usage(&format!("unexpected argument {other:?}")),
        }
    }
    let Some(campaign_path) = campaign_path else {
        return fail_usage("run needs a <campaign.json>");
    };

    // Conventional layout: e2e/campaigns/<id>.json → the e2e dir is two up.
    let e2e_dir = e2e_dir.unwrap_or_else(|| {
        campaign_path
            .parent()
            .and_then(Path::parent)
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("e2e"))
    });
    let runs_root = runs_root.unwrap_or_else(|| e2e_dir.join("runs"));
    let ts = ts.unwrap_or_else(|| {
        let secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        format!("run-{secs}")
    });

    let mut spec = match run::load_spec(&e2e_dir, &campaign_path, runs_root, ts) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("e2e run: {e}");
            return 2;
        }
    };
    if let Err(code) = apply_filters(&mut spec, &cases, &infras) {
        return code;
    }

    println!(
        "campaign {:?}: {} case(s) × {} infra shape(s) → {}",
        spec.campaign.id,
        spec.campaign.cases.len(),
        spec.campaign.infra_shapes.len(),
        spec.runs_root.join(&spec.campaign.id).join(&spec.ts).display(),
    );

    let result = match run::run_blocking(&spec) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("e2e run: {e}");
            return 2;
        }
    };

    // Summary table.
    let width = result
        .index
        .cells
        .iter()
        .map(|c| c.dir.len())
        .max()
        .unwrap_or(4)
        .max(4);
    println!("\n{:<width$}  verdict", "cell");
    for cell in &result.index.cells {
        let verdict = match (&cell.passed, &cell.error) {
            (true, _) => "PASS".to_string(),
            (false, None) => "FAIL".to_string(),
            (false, Some(e)) => format!("CRASH ({})", e.lines().next().unwrap_or("")),
        };
        println!("{:<width$}  {verdict}", cell.dir);
    }
    let failed: Vec<_> = result.index.cells.iter().filter(|c| !c.passed).collect();
    println!(
        "\n{}/{} cell(s) passed — {}",
        result.index.cells.len() - failed.len(),
        result.index.cells.len(),
        result.run_dir.join("campaign.json").display()
    );
    if failed.is_empty() { 0 } else { 1 }
}

/// `--case`/`--infra` subset the campaign; naming something the campaign does
/// not contain is an error (a typo must not silently run nothing).
fn apply_filters(spec: &mut CampaignSpec, cases: &[String], infras: &[String]) -> Result<(), i32> {
    for c in cases {
        if !spec.campaign.cases.contains(c) {
            return Err(fail_usage(&format!(
                "--case {c:?} is not in campaign {:?} (cases: {:?})",
                spec.campaign.id, spec.campaign.cases
            )));
        }
    }
    for i in infras {
        if !spec.campaign.infra_shapes.contains(i) {
            return Err(fail_usage(&format!(
                "--infra {i:?} is not in campaign {:?} (infra shapes: {:?})",
                spec.campaign.id, spec.campaign.infra_shapes
            )));
        }
    }
    if !cases.is_empty() {
        spec.campaign.cases.retain(|c| cases.contains(c));
    }
    if !infras.is_empty() {
        spec.campaign.infra_shapes.retain(|i| infras.contains(i));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// e2e validate
// ---------------------------------------------------------------------------

fn cmd_validate(args: &[String]) -> i32 {
    if args.is_empty() {
        return fail_usage("validate needs at least one <file.json>");
    }
    let mut failures = 0;
    for file in args {
        let path = Path::new(file);
        match validate_file(path) {
            Ok(kind) => println!("{file}: OK ({kind})"),
            Err(e) => {
                eprintln!("{file}: INVALID\n  {e}");
                failures += 1;
            }
        }
    }
    if failures == 0 { 0 } else { 1 }
}

/// Detect the doc type by which schema parses it (all four use
/// `deny_unknown_fields`, so exactly one structural shape fits), then run the
/// deep validation a parse alone cannot do (test case ↔ registry).
fn validate_file(path: &Path) -> Result<&'static str, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("read: {e}"))?;

    if let Ok(case) = serde_json::from_str::<model::TestCase>(&text) {
        // Check sets live beside the case in the conventional layout.
        let check_sets = path
            .parent()
            .and_then(Path::parent)
            .map(|e2e| model::load_check_sets(&e2e.join("checksets")))
            .transpose()
            .map_err(|e| e.to_string())?
            .unwrap_or_default();
        model::validate_case(&case, &e2e_core::shapes::registry(), &check_sets)
            .map_err(|e| e.to_string())?;
        return Ok("test case");
    }
    if serde_json::from_str::<model::CheckSet>(&text).is_ok() {
        return Ok("check set");
    }
    if serde_json::from_str::<model::Campaign>(&text).is_ok() {
        return Ok("campaign");
    }
    if serde_json::from_str::<e2e_core::EndpointConfig>(&text).is_ok() {
        return Ok("endpoint config");
    }
    // None matched: re-parse as the most likely kind for a useful error.
    let err = serde_json::from_str::<model::TestCase>(&text).unwrap_err();
    Err(format!("matches no authored doc type; as a test case: {err}"))
}

// ---------------------------------------------------------------------------
// e2e schema
// ---------------------------------------------------------------------------

fn cmd_schema(args: &[String]) -> i32 {
    let mut out: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--out" => match it.next() {
                Some(v) => out = Some(PathBuf::from(v)),
                None => return fail_usage("--out needs a value"),
            },
            other => return fail_usage(&format!("unexpected argument {other:?}")),
        }
    }
    let dir = out.unwrap_or_else(|| PathBuf::from("e2e/schemas"));
    if let Err(e) = write_schemas(&dir) {
        eprintln!("e2e schema: {e}");
        return 2;
    }
    0
}

/// Shared with xtask's `e2e-schema`: one file per authored doc type.
pub fn write_schemas(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for (stem, schema) in model::schemas() {
        let path = dir.join(format!("{stem}.schema.json"));
        let json = serde_json::to_string_pretty(&schema)
            .map_err(|e| format!("serialize {stem} schema: {e}"))?;
        std::fs::write(&path, json + "\n").map_err(|e| format!("write {}: {e}", path.display()))?;
        eprintln!("e2e schema: {stem:<16} -> {}", path.display());
    }
    Ok(())
}
