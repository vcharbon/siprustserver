//! The run's `inject` nodes (§6.6): each handed to the lane's injector once its
//! `after` and dwell hold, its answer awaited beside the flow, so the call's
//! legs keep their own traffic while the lane performs the action.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::stream::{FuturesUnordered, StreamExt};
use pivot_schema::bundle::Failure;
use pivot_schema::flow::Inject;
use tokio::time::Instant;

use super::Runner;
use crate::injector::InjectRequest;

/// One answer of the lane's injector: the node's id and what it said.
pub(super) type Answer = (String, Result<(), String>);

/// The injects handed to the lane's injector and not answered yet.
#[derive(Default)]
pub(super) struct Injections {
    /// Node id → what the run awaits of it.
    handed: BTreeMap<String, Handed>,
    answers: FuturesUnordered<Pin<Box<dyn Future<Output = Answer> + Send>>>,
}

/// An inject the run awaits the answer of.
struct Handed {
    action: String,
    within: Duration,
    answer_by: Instant,
}

impl Injections {
    /// The next answer; pends while none is awaited.
    pub(super) async fn next(&mut self) -> Answer {
        match self.answers.next().await {
            Some(answer) => answer,
            None => std::future::pending().await,
        }
    }
}

impl<'a, 'p> Runner<'a, 'p> {
    /// Hand every open inject whose dwell elapsed to the lane's injector. On a
    /// lane with none, an open inject stops the run by name. `false` once the
    /// run stopped.
    pub(super) fn hand_injects(&mut self) -> bool {
        let open: Vec<&'p Inject> = self
            .instance
            .cursor()
            .open_injects()
            .into_iter()
            .filter(|inject| !self.injections.handed.contains_key(&inject.id))
            .collect();
        if open.is_empty() {
            return true;
        }
        let Some(injector) = self.lane.injector.clone() else {
            return self.refuse_injects(&open);
        };
        let now = Instant::now();
        for inject in open {
            if !self.inject_due(inject).is_some_and(|at| at <= now) {
                continue;
            }
            let within = injector.within();
            self.injections.handed.insert(
                inject.id.clone(),
                Handed { action: inject.action.clone(), within, answer_by: now + within },
            );
            let injection = injector.inject(InjectRequest {
                node: inject.id.clone(),
                action: inject.action.clone(),
                target: inject.target.clone(),
            });
            let node = inject.id.clone();
            self.injections.answers.push(Box::pin(async move { (node, injection.await) }));
        }
        true
    }

    /// An `inject` the lane cannot perform stops the run by name.
    fn refuse_injects(&mut self, open: &[&Inject]) -> bool {
        for inject in open {
            self.instance.fail(Failure::InjectorMissing {
                node: inject.id.clone(),
                action: inject.action.clone(),
            });
        }
        let first = open.first().map(|inject| inject.id.as_str());
        self.end_script(None, first);
        false
    }

    /// When an inject's dwell elapses: its anchor's own time plus `delay.ms`,
    /// at once where it states none, `None` while its anchor has not happened.
    fn inject_due(&self, inject: &Inject) -> Option<Instant> {
        match &inject.delay {
            None => Some(self.started),
            Some(delay) => Some(self.anchor_at(&delay.from)? + Duration::from_millis(delay.ms)),
        }
    }

    /// The next moment an inject needs the loop: a dwell elapsing, or an
    /// answer's bound.
    pub(super) fn next_inject_wake(&self) -> Option<Instant> {
        let dwells = self
            .instance
            .cursor()
            .open_injects()
            .into_iter()
            .filter(|inject| !self.injections.handed.contains_key(&inject.id))
            .filter_map(|inject| self.inject_due(inject));
        let bounds = self.injections.handed.values().map(|handed| handed.answer_by);
        dwells.chain(bounds).min()
    }

    /// The injector's answer for `node`: performed, the node completes and
    /// what waits on it opens; refused, the run fails by name and its script
    /// ends. `false` once the run stopped.
    pub(super) fn injected(&mut self, (node, answer): Answer) -> bool {
        let Some(handed) = self.injections.handed.remove(&node) else { return true };
        match answer {
            Ok(()) => {
                self.instance.complete_inject(&node);
                self.settle_blocks(Instant::now());
                true
            }
            Err(reason) => {
                self.instance.fail(Failure::InjectRefused {
                    node: node.clone(),
                    action: handed.action,
                    reason,
                });
                self.end_script(None, Some(&node));
                false
            }
        }
    }

    /// An inject whose answer did not come within the injector's bound fails
    /// the run by name and ends its script. `false` once the run stopped.
    pub(super) fn expire_injects(&mut self) -> bool {
        let now = Instant::now();
        let expired = self
            .injections
            .handed
            .iter()
            .find(|(_, handed)| handed.answer_by <= now)
            .map(|(node, handed)| (node.clone(), handed.action.clone(), handed.within));
        let Some((node, action, within)) = expired else { return true };
        self.instance.fail(Failure::InjectTimedOut {
            node: node.clone(),
            action,
            within_ms: within.as_millis() as u64,
        });
        self.end_script(None, Some(&node));
        false
    }
}
