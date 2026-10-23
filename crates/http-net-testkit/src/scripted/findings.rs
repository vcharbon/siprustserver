//! The findings a [`ScriptedHttpService`] records, indexed when recorded by
//! the instances they name, and the reads of them: the whole service's, a few
//! instances' (their own findings only), and those naming no instance.

use std::collections::{HashMap, HashSet};

use super::service::{Instance, Shared, State};
use super::verdict::{HttpFinding, HttpFindingKind, HttpVerdict};
use super::ScriptedHttpService;

/// Every finding in the order recorded, and where each instance's sit.
#[derive(Default)]
pub(super) struct Findings {
    all: Vec<HttpFinding>,
    by_instance: HashMap<u64, Vec<usize>>,
    unattributed: Vec<usize>,
}

impl Findings {
    pub(super) fn record(&mut self, finding: HttpFinding) {
        let at = self.all.len();
        if finding.instances.is_empty() {
            self.unattributed.push(at);
        }
        for instance in &finding.instances {
            let indices = self.by_instance.entry(*instance).or_default();
            if indices.last() != Some(&at) {
                indices.push(at);
            }
        }
        self.all.push(finding);
    }

    /// Where `instance`'s findings sit, in the order recorded.
    fn naming(&self, instance: u64) -> &[usize] {
        self.by_instance.get(&instance).map_or(&[], Vec::as_slice)
    }
}

impl State {
    fn instance(&self, instance: u64) -> Option<&Instance> {
        usize::try_from(instance).ok().and_then(|i| self.instances.get(i))
    }
}

impl Shared {
    pub(super) fn verdict(&self, instance: u64) -> HttpVerdict {
        let state = self.state.lock().unwrap();
        let Some(inst) = state.instance(instance) else {
            return HttpVerdict { instance, opened: false, complete: false, findings: Vec::new() };
        };
        let mut findings: Vec<HttpFinding> = state
            .findings
            .naming(instance)
            .iter()
            .map(|&at| state.findings.all[at].clone())
            .collect();
        let complete = inst.progress.complete();
        if !complete {
            findings.push(unserved(instance, inst));
        }
        HttpVerdict { instance, opened: inst.opened, complete, findings }
    }
}

fn unserved(instance: u64, inst: &Instance) -> HttpFinding {
    HttpFinding {
        kind: HttpFindingKind::Unserved,
        instances: vec![instance],
        method: String::new(),
        path: String::new(),
        body: String::new(),
        detail: format!(
            "{} {}: {}",
            inst.script.open.method,
            inst.script.open.path,
            inst.progress.describe(inst.opened)
        ),
    }
}

impl ScriptedHttpService {
    /// Every finding so far, plus one [`HttpFindingKind::Unserved`] per
    /// instance not complete: read at run end, the service-level verdict.
    pub fn findings(&self) -> Vec<HttpFinding> {
        let state = self.shared.state.lock().unwrap();
        let mut findings = state.findings.all.clone();
        for (i, inst) in state.instances.iter().enumerate() {
            if !inst.progress.complete() {
                findings.push(unserved(i as u64, inst));
            }
        }
        findings
    }

    /// The findings naming any of `instances`, each once, instance by instance
    /// in the order given and then recorded, each instance's
    /// [`HttpFindingKind::Unserved`] after its own: what their verdicts state,
    /// read without touching the rest of the service's findings.
    pub fn findings_of(&self, instances: &[u64]) -> Vec<HttpFinding> {
        let state = self.shared.state.lock().unwrap();
        let mut out = Vec::new();
        let mut taken = HashSet::new();
        for &instance in instances {
            let Some(inst) = state.instance(instance) else {
                continue;
            };
            for &at in state.findings.naming(instance) {
                if taken.insert(at) {
                    out.push(state.findings.all[at].clone());
                }
            }
            if !inst.progress.complete() {
                out.push(unserved(instance, inst));
            }
        }
        out
    }

    /// The findings naming no instance, in the order recorded.
    pub fn unattributed_findings(&self) -> Vec<HttpFinding> {
        let state = self.shared.state.lock().unwrap();
        state.findings.unattributed.iter().map(|&at| state.findings.all[at].clone()).collect()
    }
}
