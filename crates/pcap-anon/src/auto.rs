//! One capture's ingest, with no human in the loop: the flows document is
//! anonymised and its callflow written. Whether it landed or why it did not
//! is DATA in the returned [`AutoSummary`], never a process failure: the
//! ingest classifies captures from it.
//!
//! What is written next to the summary: `<capture>.anon.flows.json` and
//! `source.txt` (the agent-readable callflow).
//!
//! The capture is anonymised BEFORE anything is persisted, so a work dir holds
//! anonymised data only and a production call can seed a non-regression suite
//! without a raw-identity copy existing. The profile's scope
//! ([`crate::profile::Profile::scope`]) keeps the addresses it names as the
//! SUT — the cut matches the document against them — and the texts the SUT
//! writes. The seed is the caller's; the summary carries the mapping's counts
//! and never the seed. Under `verify`, a number, a display name or a name of
//! the raw document surviving in the anonymised one is a capture-level error
//! and nothing but the summary is persisted.

use std::path::Path;

use serde::Serialize;

use crate::anonymize::names::NameCounts;
use crate::fake::Seed;
use crate::reading::Reading;
use crate::{anonymize, model, plan, profile, propose, text, verify};

/// Per-capture machine-readable outcome, written as `auto.json`.
#[derive(Debug, Serialize)]
pub struct AutoSummary {
    pub capture: String,
    /// Capture-level failure (ingest, anonymisation, verification); absent
    /// when the capture landed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The anonymization pass's counts (absent when it did not run).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub anonymized: Option<AnonymizeStats>,
}

/// What the anonymization pass mapped: identities rewritten and the collisions
/// among their fakes ([`anonymize::Report`]); the seed stays out of the file.
#[derive(Debug, Serialize)]
pub struct AnonymizeStats {
    pub identities: usize,
    /// Keys no form of the plan recognised, mapped whole.
    pub unrecognised: usize,
    pub shared_keys: usize,
    pub nested_fakes: usize,
    pub merged_call_ids: usize,
    /// Distinct User-to-User payloads in an encoding other than hex, left
    /// as found.
    pub uui_skipped: usize,
    /// Distinct hex User-to-User payloads that decode to no text, redacted.
    pub uui_redacted: usize,
    /// Distinct host names, charging ids, product texts and addresses mapped,
    /// and how many of them share their fake with another.
    pub hosts: usize,
    pub charging_ids: usize,
    pub texts: usize,
    pub addresses: usize,
    pub name_collisions: usize,
    /// Values redacted whole (a PASSporT), and body parts of a media type the
    /// anonymiser does not understand, redacted whole.
    pub redacted: usize,
    pub redacted_parts: usize,
    /// Every check's count ([`verify::verdict`]); absent when the checks did
    /// not run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verified: Option<verify::verdict::Verdict>,
}

/// What an ingest runs under, as the caller states it: the source network's
/// numbering plan, its profile, and the reading its captures were extracted
/// under.
#[derive(Debug)]
pub struct Config {
    pub plan: plan::Plan,
    pub profile: profile::Profile,
    pub reading: Reading,
}

impl Config {
    /// The plan, profile and reading the three files state, or why one of
    /// them does not read.
    pub fn read(plan: &Path, profile: &Path, reading: &Path) -> Result<Config, String> {
        Ok(Config {
            plan: plan::Plan::from_json_str(&read_file(plan, "plan")?)?,
            profile: profile::Profile::from_json_str(&read_file(profile, "profile")?)?,
            reading: Reading::stated(
                &read_file(reading, "reading")?,
                &reading.display().to_string(),
            )?,
        })
    }
}

/// The text of the `what` file at `path`.
pub fn read_file(path: &Path, what: &str) -> Result<String, String> {
    std::fs::read_to_string(path).map_err(|e| format!("cannot read {what} {}: {e}", path.display()))
}

/// Run the ingest of one capture's flows JSON into `out` under `config`. IO
/// errors on the artifacts are hard errors; everything semantic is summary
/// data. The caller persists the summary (`write_summary`).
pub fn run(
    flows_json: &str,
    capture: &str,
    out: &Path,
    config: &Config,
    seed: &Seed,
    verify: bool,
) -> Result<AutoSummary, String> {
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    let failed = |error: String, anonymized: Option<AnonymizeStats>| AutoSummary {
        capture: capture.to_string(),
        error: Some(error),
        anonymized,
    };
    let mut model = match model::ingest_str(flows_json) {
        Ok(m) => m,
        Err(e) => return Ok(failed(format!("flows ingest failed: {e}"), None)),
    };
    let Config { plan, profile, reading } = config;
    let scope = profile.scope();
    let (mut anonymized, report) = match anonymize::anonymize_in(&mut model, plan, seed, &scope) {
        Ok(report) => (stats(&report), report),
        // An anonymised input is no capture to ingest: refused, not summarised.
        Err(e @ anonymize::AnonymizeError::AlreadyRewritten(_)) => return Err(e.to_string()),
        Err(e) => return Ok(failed(format!("anonymize failed: {e}"), None)),
    };
    // The leak check reads the raw document again; a survivor means nothing
    // of the capture is persisted beyond the summary's counts.
    if verify {
        let raw = model::ingest_str(flows_json).map_err(|e| format!("flows ingest failed: {e}"))?;
        let verdict = verify::verdict::judge(&raw, &model, plan, &scope, reading, &report);
        anonymized.verified = Some(verdict);
        if verdict.total() > 0 {
            return Ok(failed(verdict.refusal(), Some(anonymized)));
        }
    }
    landed(&model, capture, out, reading, Some(anonymized))
}

/// The ingest of one capture's flows JSON into `out` with the document TAKEN
/// AS IS: no anonymisation, no seed, no verification. For a document the
/// inbox anonymised from the raw read under its own seed, or a lab capture
/// whose owner chose to keep it unanonymised; what lands is that document and
/// its callflow, the document re-derived under `reading`.
pub fn run_as_is(
    flows_json: &str,
    capture: &str,
    out: &Path,
    reading: &Reading,
) -> Result<AutoSummary, String> {
    std::fs::create_dir_all(out).map_err(|e| format!("cannot create {}: {e}", out.display()))?;
    match model::ingest_str(flows_json) {
        Ok(model) => landed(&model, capture, out, reading, None),
        Err(e) => Ok(AutoSummary {
            capture: capture.to_string(),
            error: Some(format!("flows ingest failed: {e}")),
            anonymized: None,
        }),
    }
}

/// `model` written into `out` as the capture's kept document and callflow.
fn landed(
    model: &model::Flows,
    capture: &str,
    out: &Path,
    reading: &Reading,
    anonymized: Option<AnonymizeStats>,
) -> Result<AutoSummary, String> {
    write(out, &format!("{capture}.anon.flows.json"), &model::to_json_string(model, reading)?)?;
    let proposals = propose::propose(model);
    write(out, "source.txt", &text::render(capture, model, &proposals))?;
    Ok(AutoSummary { capture: capture.to_string(), error: None, anonymized })
}

/// The summary's counts of one anonymisation.
fn stats(report: &anonymize::Report) -> AnonymizeStats {
    let names = NameCounts::of(&report.names);
    AnonymizeStats {
        identities: report.entries.len(),
        unrecognised: report.unrecognised(),
        shared_keys: report.shared_keys(),
        nested_fakes: report.nested_fakes(),
        merged_call_ids: report.merged_call_ids,
        uui_skipped: report.uui_skipped,
        uui_redacted: report.uui_redacted,
        hosts: names.hosts,
        charging_ids: names.icids,
        texts: names.texts,
        addresses: names.addresses,
        name_collisions: report.name_collisions,
        redacted: names.redacted,
        redacted_parts: report.redacted_parts,
        verified: None,
    }
}

/// Serialize + write the summary as `<out>/auto.json`.
pub fn write_summary(summary: &AutoSummary, out: &Path) -> Result<(), String> {
    let json = serde_json::to_string_pretty(summary).map_err(|e| e.to_string())?;
    write(out, "auto.json", &json)
}

fn write(out: &Path, name: &str, content: &str) -> Result<(), String> {
    let path = out.join(name);
    std::fs::write(&path, content).map_err(|e| format!("cannot write {}: {e}", path.display()))
}
