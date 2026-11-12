//! The post-run RFC audit a lane hands back, and the `rfc.json` it leaves
//! beside `verdict.json`. One concern: turning the harness's finding objects
//! into the bundle contract — attributing each to the party the rule charges,
//! and ruling whether it gates — and writing it.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::Path;

use pivot_schema::bundle::{locate, RecordedMessage, RfcFinding, RunRfcAudit};
use pivot_schema::placement::{Endpoint, Side};
use sip_net_testkit::RecordedSipEntry;

use crate::Failure;

/// What the document says about a finding charged to one of its endpoints:
/// which actor it names, and the statement that cancels it (§11.1).
pub trait Statements {
    /// The actor a finding charged to `endpoint` names, read through the leg
    /// that carries `offending`'s call — several actors may share an endpoint.
    fn actor_named(&self, endpoint: &str, offending: &[u8]) -> Option<String>;
    /// The document's statement of `rule` against `actor` on the transaction
    /// `offending` rides, where it makes one.
    fn cancelled_by(&self, rule: &str, actor: &str, offending: &[u8]) -> Option<String>;
    /// The document's attribution of the system under test's finding of
    /// `rule` on the transaction `offending` rides to the party that
    /// originated it, where it states both halves.
    fn relayed(&self, rule: &str, offending: &[u8]) -> Option<crate::cancellation::Relayed>;
    /// Why the document cannot state its scripted parties' violations, where
    /// it says it never read its captured trace for them.
    fn unread(&self) -> Option<String>;
    /// The run's recording, by leg, the statements are read against.
    fn legs(&self) -> &BTreeMap<String, Vec<RecordedMessage>>;
}

impl Statements for crate::cancellation::Statements {
    fn actor_named(&self, endpoint: &str, offending: &[u8]) -> Option<String> {
        crate::cancellation::Statements::actor_named(self, endpoint, offending)
    }
    fn cancelled_by(&self, rule: &str, actor: &str, offending: &[u8]) -> Option<String> {
        self.cancelling(rule, actor, offending)
    }
    fn relayed(&self, rule: &str, offending: &[u8]) -> Option<crate::cancellation::Relayed> {
        crate::cancellation::Statements::relayed(self, rule, offending)
    }
    fn unread(&self) -> Option<String> {
        crate::cancellation::Statements::unread(self)
    }
    fn legs(&self) -> &BTreeMap<String, Vec<RecordedMessage>> {
        crate::cancellation::Statements::legs(self)
    }
}

/// The full suite's findings (advisory included), each attributed to the
/// party the rule holds responsible, with what each one fails marked.
///
/// The culprit is the rule's own `charged` bind — the one that emitted the
/// offending message or owed the missing one — never the sender of the
/// offending entry, which for an omission (an un-ACKed 2xx) is the other
/// party. A rule that names no culprit falls back to that sender; a culprit
/// the rule names is never replaced by it. A labelled bind's culprit is the
/// endpoint its label names (`ip:port#<endpoint id>`). A culprit that is a
/// document endpoint is named by the actor `statements` reads off the
/// offending call, or by the endpoint id where no leg names one.
///
/// Only a finding the harness's own hard gate returned (non-advisory,
/// unwaived) can fail the cell, and only the system under test's GATES (§11.1).
/// A document actor's is the scripted party's: the document's statement of the
/// same violation, by the same actor, on the same transaction cancels it
/// (`cancelled_by`); unstated, it is a source fault, or `unjudged` where the
/// document never read its captured trace. A finding charged to no document
/// endpoint — the SUT's — is cancelled only where the document states it
/// relayed onward from a party AND the run holds that party's finding of the
/// rule, cancelled by its own statement (`caused_by`). Each offending entry is
/// also named as the run's recording holds it (`offending_message`), so a
/// reader of the recording finds the one message the audit means.
pub fn audited(
    all: &[sip_net_testkit::RfcFinding],
    gating: &[sip_net_testkit::RfcFinding],
    entries: &[RecordedSipEntry],
    actors: &Actors,
    statements: &dyn Statements,
) -> RunRfcAudit {
    let unread = statements.unread();
    let placed = locate(entries.iter().map(|e| e.raw.as_slice()), statements.legs());
    let read: Vec<(RfcFinding, Option<crate::cancellation::Relayed>)> = all
        .iter()
        .map(|f| {
            let offending = f.offending.and_then(|i| i.checked_sub(1)).and_then(|i| entries.get(i));
            let culprit = match f.charged.as_deref() {
                Some(bind) => Culprit::of_bind(bind, actors),
                None => Culprit::of_sender(offending.map(|e| e.from), actors),
            };
            let actor = culprit.actor(offending, statements);
            let relayed = match (&actor, offending) {
                (None, Some(entry)) => statements.relayed(&f.rule, &entry.raw),
                _ => None,
            };
            let cancelled = actor
                .as_deref()
                .zip(offending)
                .and_then(|(actor, entry)| statements.cancelled_by(&f.rule, actor, &entry.raw));
            let standing = gating.contains(f) && cancelled.is_none();
            let scripted = actor.is_some();
            let finding = RfcFinding {
                rule: f.rule.clone(),
                lane: f.lane.clone(),
                detail: f.detail.clone(),
                advisory: f.advisory,
                gating: standing && !scripted,
                offending: f.offending,
                offending_message: f
                    .offending
                    .and_then(|i| i.checked_sub(1))
                    .and_then(|i| placed.get(i).cloned().flatten()),
                charged: culprit.charged,
                actor,
                cancelled_by: cancelled,
                caused_by: None,
                source_fault: standing && scripted && unread.is_none(),
                unjudged: if standing && scripted { unread.clone() } else { None },
            };
            (finding, relayed)
        })
        .collect();
    // A relayed statement cancels the SUT's finding only where this run shows
    // the stated party breaking the rule too, cancelled by its own statement.
    let committed: Vec<(String, String)> = read
        .iter()
        .filter(|(f, _)| f.cancelled_by.is_some())
        .filter_map(|(f, _)| f.actor.clone().map(|actor| (f.rule.clone(), actor)))
        .collect();
    let findings = read
        .into_iter()
        .map(|(mut finding, relayed)| {
            let Some(relayed) = relayed else { return finding };
            if committed
                .iter()
                .any(|(rule, actor)| *rule == finding.rule && *actor == relayed.caused_by)
            {
                finding.gating = false;
                finding.cancelled_by = Some(relayed.label);
                finding.caused_by = Some(relayed.caused_by);
            }
            finding
        })
        .collect();
    RunRfcAudit::Audited { findings }
}

/// The party a finding is charged to: the bind `rfc.json` writes as charged,
/// and the document endpoints it may be, in id order.
struct Culprit {
    charged: Option<String>,
    endpoints: Vec<String>,
}

impl Culprit {
    /// The bind the rule charged. A labelled bind is the sub-lane
    /// `ip:port#<endpoint id>`, and its label names the one endpoint whatever
    /// else shares the socket; a plain bind may be any endpoint on its socket.
    /// The bind is written as the rule wrote it; one naming no socket names no
    /// endpoint.
    fn of_bind(bind: &str, actors: &Actors) -> Culprit {
        let addr = sip_net_testkit::report::lane_addr(bind);
        let endpoints = match bind.split_once('#') {
            Some((_, label)) => actors.ids.get(label).cloned().into_iter().collect(),
            None => addr.map(|a| actors.at(a)).unwrap_or_default(),
        };
        Culprit { charged: Some(bind.to_string()), endpoints }
    }

    /// The offending entry's sender, for a rule that names no culprit.
    fn of_sender(sender: Option<SocketAddr>, actors: &Actors) -> Culprit {
        Culprit {
            charged: sender.map(|a| a.to_string()),
            endpoints: sender.map(|a| actors.at(a)).unwrap_or_default(),
        }
    }

    /// The actor the finding names: the first endpoint whose leg carries the
    /// offending call names its actor, else the first endpoint by its id. Two
    /// endpoints on one plain bind whose legs carry the same Call-ID name the
    /// first of them.
    fn actor(
        &self,
        offending: Option<&RecordedSipEntry>,
        statements: &dyn Statements,
    ) -> Option<String> {
        offending
            .and_then(|entry| {
                self.endpoints.iter().find_map(|e| statements.actor_named(e, &entry.raw))
            })
            .or_else(|| self.endpoints.first().cloned())
    }
}

/// The document's scripted peers (`Side::Peer`) as [`audited`] reads a culprit
/// in: their ids, and each socket a view places them on. A finding charged at
/// a SUT-side socket is the system under test's, never a scripted party's.
#[derive(Debug, Default)]
pub struct Actors {
    ids: BTreeSet<String>,
    by_addr: BTreeMap<SocketAddr, BTreeSet<String>>,
}

impl Actors {
    /// Every peer endpoint on `addr`, in id order.
    fn at(&self, addr: SocketAddr) -> Vec<String> {
        self.by_addr.get(&addr).map(|ids| ids.iter().cloned().collect()).unwrap_or_default()
    }
}

/// The scripted peers of `endpoints`, placed by every VIEW of where they are.
///
/// A lane that asks for port `0` is recorded, and charged, at the socket it
/// was given, while a lane binding the asked address is charged there, so the
/// asked view and the resolved one both place the same endpoint. Several
/// endpoints may share one address; a labelled bind's label tells them apart.
pub fn actors(endpoints: &[Endpoint], views: &[&BTreeMap<String, SocketAddr>]) -> Actors {
    let mut actors = Actors::default();
    for e in endpoints.iter().filter(|e| e.side == Side::Peer) {
        actors.ids.insert(e.id.clone());
        for addr in views.iter().filter_map(|v| v.get(&e.id)) {
            actors.by_addr.entry(*addr).or_default().insert(e.id.clone());
        }
    }
    actors
}

/// Write `rfc.json` into the bundle. A gating finding and a source fault NAME
/// the deviation on stderr too — a bare count triages nothing — the source
/// fault with the scripted party it charges; a finding cancelled as relayed
/// from a party names that party's statement.
pub fn record(out_dir: &Path, audit: &RunRfcAudit) -> Result<(), Failure> {
    if let RunRfcAudit::Audited { findings } = audit {
        for f in findings.iter().filter(|f| f.gating) {
            crate::diag::line(format_args!(
                "[replay] rfc finding {} [{}]: {}",
                f.rule, f.lane, f.detail
            ));
        }
        for f in findings.iter().filter(|f| f.source_fault) {
            let by = f.actor.as_deref().unwrap_or("a scripted party");
            crate::diag::line(format_args!(
                "[replay] source fault: {by} broke {} [{}], which the document does not state: {}",
                f.rule, f.lane, f.detail
            ));
        }
        for f in findings.iter().filter(|f| f.caused_by.is_some()) {
            let label = f.cancelled_by.as_deref().unwrap_or_default();
            crate::diag::line(format_args!(
                "[replay] rfc finding {} [{}] cancelled — {label}",
                f.rule, f.lane
            ));
        }
    }
    let text = serde_json::to_string_pretty(audit)
        .map_err(|e| Failure::Environment(format!("serialize rfc audit: {e}")))?;
    std::fs::write(out_dir.join("rfc.json"), text + "\n")
        .map_err(|e| Failure::Environment(format!("write rfc.json: {e}")))
}

#[cfg(test)]
#[path = "audit_tests.rs"]
mod tests;
