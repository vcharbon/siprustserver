//! The anonymiser's quick check (`pcap2test anonymize --check-reproducible`):
//! the raw document is anonymised twice under one seed, and the two outputs
//! must be byte-identical — a regeneration from the raw capture under its
//! kept seed reproduces the anonymised one — and the leak check must count
//! nothing in the output ([`crate::verify`]). The leak check alone does not
//! cover the first half: an output with no leak can still differ from run to
//! run. Every token the leak check cannot explain, and every number or name
//! of the raw document that survives, is reported by where it
//! sits in the datagram and by its shape, never by its value: a digit reads
//! `9`, a letter `a` or `A`, punctuation as written. `--sample N` reads the
//! first N legs, so the answer comes in seconds on any capture.

use crate::anonymize::{anonymize_in, AnonymizeError};
use crate::fake::address::Scope;
use crate::fake::Seed;
use crate::identities;
use crate::model::{self, Flows};
use crate::plan::Plan;
use crate::reading::Reading;
use crate::verify;
use std::collections::{BTreeMap, HashSet};

/// One token of the output the leak check cannot explain or counts as a
/// survivor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Its class: a token no fake explains (`domain`, `address`, `word`,
    /// `non-ascii`, `digits`), or a survivor of the raw document (`number`,
    /// `name`).
    pub class: String,
    /// Where its first occurrence sits ([`identities::site_of`]).
    pub site: String,
    /// Its shape ([`shape`]).
    pub shape: String,
    /// How many datagrams of the output carry it.
    pub datagrams: usize,
}

/// What the check read and found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub legs: usize,
    pub datagrams: usize,
    /// Datagrams the two runs wrote differently.
    pub differing: usize,
    /// Whether the two runs' documents differ anywhere (a model string
    /// included).
    pub documents_differ: bool,
    /// Every check's count over the first run's output.
    pub verdict: verify::verdict::Verdict,
    pub findings: Vec<Finding>,
}

impl Report {
    /// Whether the two runs agree byte for byte and the output leaks nothing.
    pub fn holds(&self) -> bool {
        self.differing == 0
            && !self.documents_differ
            && self.verdict.total() == 0
            && self.findings.is_empty()
    }
}

/// Anonymise the raw `flows` (its first `sample` legs when given) twice under
/// `seed` and report how the runs differ and what the output, read under
/// `reading`, leaks.
pub fn check(
    mut raw: Flows,
    plan: &Plan,
    scope: &Scope,
    reading: &Reading,
    seed: &Seed,
    sample: Option<usize>,
) -> Result<Report, AnonymizeError> {
    if let Some(n) = sample {
        raw.legs.truncate(n);
    }
    let (mut once, mut twice) = (raw.clone(), raw.clone());
    let report = anonymize_in(&mut once, plan, seed, scope)?;
    anonymize_in(&mut twice, plan, seed, scope)?;
    let (a, b) = (raws(&once, scope), raws(&twice, scope));
    let differing = a.iter().zip(&b).filter(|(x, y)| x != y).count();
    let documents_differ =
        model::to_json_string(&once, reading).ok() != model::to_json_string(&twice, reading).ok();
    let mut findings: Vec<Finding> =
        verify::leaks::unexplained(&once, scope, &report.fakes, reading)
            .into_iter()
            .map(|(class, token)| finding(class, &token, &a))
            .collect();
    findings.extend(survivor_findings(&raw, &once, plan, scope));
    Ok(Report {
        legs: once.legs.len(),
        datagrams: a.len(),
        differing,
        documents_differ,
        verdict: verify::verdict::judge(&raw, &once, plan, scope, reading, &report),
        findings,
    })
}

/// One finding per surviving number (`number`) and surviving name (`name`):
/// where its first occurrence sits, its shape, and how many datagrams carry it.
fn survivor_findings(raw: &Flows, out: &Flows, plan: &Plan, scope: &Scope) -> Vec<Finding> {
    let datagram = |leg: usize, msg: usize| model::wire(&out.legs[leg].msgs[msg]);
    let site = |leg: usize, msg: usize, at: Option<usize>| match at {
        Some(at) => {
            let datagram = datagram(leg, msg);
            identities::site_of(&datagram, at.min(datagram.len()))
        }
        None => "model".to_string(),
    };
    // Per (class, value): the first site and the datagrams (leg, message) carrying it.
    type Sites = (String, HashSet<(usize, usize)>);
    let mut by: BTreeMap<(&'static str, Vec<u8>), Sites> = BTreeMap::new();
    for s in verify::survivor_sites(raw, out, plan, scope) {
        let entry = by
            .entry(("number", s.needle.into_bytes()))
            .or_insert_with(|| (site(s.leg, s.msg, Some(s.at)), HashSet::new()));
        entry.1.insert((s.leg, s.msg));
    }
    for s in verify::names::name_survivor_sites(raw, out, scope) {
        let entry = by
            .entry(("name", s.name))
            .or_insert_with(|| (site(s.leg, s.msg, s.at), HashSet::new()));
        entry.1.insert((s.leg, s.msg));
    }
    by.into_iter()
        .map(|((class, value), (site, datagrams))| Finding {
            class: class.to_string(),
            site,
            shape: shape(&String::from_utf8_lossy(&value)),
            datagrams: datagrams.len(),
        })
        .collect()
}

/// Every datagram of `flows`, its protocol text blanked under `scope`'s body
/// types.
fn raws(flows: &Flows, scope: &Scope) -> Vec<Vec<u8>> {
    let blanked = |m| crate::verify::without_protocol_text(&model::wire(m), scope.bodies());
    flows.legs.iter().flat_map(|l| l.msgs.iter().map(blanked)).collect()
}

/// Where the lowercase `token` sits in the output, and its shape: a digit
/// run where it stands whole, outside a protocol counter.
fn finding(class: &str, token: &[u8], output: &[Vec<u8>]) -> Finding {
    let at = |raw: &Vec<u8>| {
        let lower = raw.to_ascii_lowercase();
        let mut found = lower.windows(token.len().max(1)).enumerate().filter(|(_, w)| *w == token);
        found
            .find(|&(p, _)| class != "digits" || whole_run(raw, p, p + token.len()))
            .map(|(p, _)| p)
    };
    let mut sites = output.iter().filter_map(|raw| at(raw).map(|p| identities::site_of(raw, p)));
    let site = sites.next().unwrap_or_else(|| "escaped or model".to_string());
    Finding {
        class: class.to_string(),
        site,
        shape: shape(&String::from_utf8_lossy(token)),
        datagrams: output.iter().filter(|r| at(r).is_some()).count(),
    }
}

/// Whether `raw[start..end]` is a whole digit run outside a protocol counter.
fn whole_run(raw: &[u8], start: usize, end: usize) -> bool {
    let digit = |i: Option<&u8>| i.is_some_and(u8::is_ascii_digit);
    !digit(start.checked_sub(1).and_then(|i| raw.get(i)))
        && !digit(raw.get(end))
        && !crate::rewrite::fields::is_counter(raw, start, end)
}

/// A value with every digit written `9` and every ASCII letter `a` or `A`;
/// any other byte (a non-ASCII one included) `?`, punctuation as written.
pub fn shape(value: &str) -> String {
    value
        .bytes()
        .map(|b| match b {
            b'0'..=b'9' => '9',
            b'a'..=b'z' => 'a',
            b'A'..=b'Z' => 'A',
            0x20..=0x7e => char::from(b),
            _ => '?',
        })
        .collect()
}

#[cfg(test)]
mod tests;
