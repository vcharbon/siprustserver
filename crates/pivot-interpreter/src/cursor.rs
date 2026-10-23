//! The **scheduler** (`PCAP2TEST_PIVOT_V3.md` §6, §14 item 2): which steps the
//! run is on right now.
//!
//! Two ordering rules, and no third. Same-leg order is LIST order; cross-leg and
//! cross-call order is `after`. List order binds within one transaction and
//! between a send and what it provokes; it never binds where nothing could
//! establish it — a relay standing behind a send (§6.7b) and an answer to a
//! transaction this leg has already opened (§6.7c) arm beside the step in
//! front of them, and the answers a CANCEL this leg received draws (§6.7e) go
//! out past it. Everything else here is what the three nondeterminism
//! constructs mean at run time:
//!
//! - **`alt`** — every branch's first message is armed at once; the first one to
//!   arrive COMMITS the alt, and the run never backtracks. The other branches'
//!   steps are discarded, which is why nothing outside the alt may reference
//!   them.
//! - **`optional`** — a tolerated absence never blocks its leg. It stays armed
//!   beside the steps behind it, and is RELEASED the moment a later step on the
//!   same leg MATCHES, unless a pending required expect or unsent send stands
//!   in front of it. A send releases nothing: the runner decides when it
//!   sends, so a send overtaking a tolerated expect says nothing about a
//!   message still in flight toward it.
//! - **`unordered`** — every member is armed together and the group completes
//!   when all of them have arrived; no member is earlier than another, so none
//!   may reference another.
//!
//! A dwell is SLEPT only on a SEND: an expect's timing is its arrival, so
//! `delay` on an expect gates no match. It does gate the BUDGET, which never
//! opens before the dwell it declares — so an expect never spends `within_ms`
//! across the very gap the document measured.

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::{CompiledStep, Plan};
use crate::program::ItemKind;

mod transaction;

/// Where one step stands; every status but `Pending` is settled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Not yet done: waiting to be armed, armed, or in flight.
    Pending,
    /// Sent, or matched by an arriving datagram.
    Complete,
    /// An `optional` expect a later step on its leg overtook.
    Released,
    /// A required expect a FINAL on its own transaction retired: the status it
    /// carried was charged on the step, and no other rides that transaction
    /// again (RFC 3261 §17.1.3).
    Retired,
    /// A step of an `alt` branch that did not run.
    Discarded,
}

impl StepStatus {
    /// Whether the step will never be waited for again.
    pub fn settled(self) -> bool {
        !matches!(self, StepStatus::Pending)
    }
}

/// Where one top-level item stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ItemStatus {
    Pending,
    Complete,
}

/// The run's position in the flow.
#[derive(Debug, Clone)]
pub struct Cursor<'p> {
    plan: &'p Plan,
    steps: BTreeMap<String, StepStatus>,
    items: Vec<ItemStatus>,
    /// `alt` item index → the branch index that committed.
    committed: BTreeMap<usize, usize>,
    /// `inject` item index → whether the lane's injector has been called.
    injected: BTreeSet<usize>,
    /// Completion order, for the verdict.
    completed: Vec<String>,
    released: Vec<String>,
    retired: Vec<String>,
}

impl<'p> Cursor<'p> {
    pub fn new(plan: &'p Plan) -> Self {
        let steps = plan.steps().iter().map(|s| (s.id.clone(), StepStatus::Pending)).collect();
        let items = vec![ItemStatus::Pending; plan.program().items.len()];
        Cursor {
            plan,
            steps,
            items,
            committed: BTreeMap::new(),
            injected: BTreeSet::new(),
            completed: Vec::new(),
            released: Vec::new(),
            retired: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn status(&self, step: &str) -> Option<StepStatus> {
        self.steps.get(step).copied()
    }

    pub fn committed_branch(&self, alt_id: &str) -> Option<&str> {
        let item = self.plan.program().item_for(alt_id)?;
        let branch = *self.committed.get(&item.index)?;
        item.branches.get(branch).map(|b| b.name.as_str())
    }

    pub fn completed(&self) -> &[String] {
        &self.completed
    }

    pub fn released(&self) -> &[String] {
        &self.released
    }

    pub fn retired(&self) -> &[String] {
        &self.retired
    }

    /// Whether a node id — a step's or a block's — has completed. This is what
    /// `after` waits on.
    pub fn node_complete(&self, node: &str) -> bool {
        if let Some(status) = self.steps.get(node) {
            return matches!(
                status,
                StepStatus::Complete | StepStatus::Released | StepStatus::Retired
            );
        }
        self.plan
            .program()
            .item_for(node)
            .is_some_and(|item| self.items[item.index] == ItemStatus::Complete)
    }

    /// Every item's `after` is satisfied.
    fn item_open(&self, index: usize) -> bool {
        let item = &self.plan.program().items[index];
        item.after.iter().all(|node| self.node_complete(node))
    }

    /// Whether an item's steps declare a race with one already armed on this leg.
    ///
    /// `overlap` is symmetric — "this step and the named one may arrive in
    /// either order" (§6.1) — so either side may carry the field and the reading
    /// is the same. A race outlives a member that settled first: a step naming
    /// a completed one races whatever that one raced ([`Self::races_of`]).
    fn races_with(&self, frontier: &[String], armed: &[String]) -> bool {
        let names = |a: &str, b: &str| self.races_of(a).contains(&b);
        frontier.iter().any(|id| armed.iter().any(|prev| names(id, prev) || names(prev, id)))
    }

    /// The steps `step` races: the one its `overlap` names and, while each
    /// named step has already completed, the one that step names in turn.
    fn races_of(&self, step: &str) -> Vec<&'p str> {
        let mut out: Vec<&'p str> = Vec::new();
        let mut at = self.plan.step(step).and_then(|s| s.overlap.as_deref());
        while let Some(named) = at.filter(|n| !out.contains(n) && *n != step) {
            out.push(named);
            if self.steps.get(named).copied() != Some(StepStatus::Complete) {
                break;
            }
            at = self.plan.step(named).and_then(|s| s.overlap.as_deref());
        }
        out
    }

    /// Whether every step of an item is an arrival some OTHER leg has already
    /// caused: an `expect` measured at a SYNTHETIC ZERO from a completed step on
    /// another leg (§6.8).
    ///
    /// That shape is a relay, and the zero is the tell: a hop's latency belongs
    /// to whichever system replays the capture, so the document states none of
    /// it (§6.7a). The message may therefore already be on the wire, and
    /// ordering it behind a SEND this leg has not made yet orders an arrival
    /// behind a decision that cannot cause it — any system quicker than the
    /// captured one by that latency then delivers it to a leg waiting for
    /// something else.
    ///
    /// A cross-leg anchor carrying a REAL dwell is a duration the document does
    /// hold, and §6.7a's closing rule stands for it: two dwells measured from
    /// two anchors are both the document's and their order stands. So the zero
    /// and the absent timer are part of the test, not decoration.
    ///
    /// It arms only where the leg can TELL IT APART: an EXPECT already armed
    /// here with this discriminator would let the earlier step's datagram settle
    /// the later one. A send with the same discriminator cannot — nothing armed
    /// consumes a datagram but an expect (`exec::deliver`).
    fn caused_elsewhere(&self, frontier: &[String], armed: &[String]) -> bool {
        !frontier.is_empty()
            && frontier.iter().all(|id| {
                let Some(step) = self.plan.step(id) else { return false };
                if !step.is_expect() || step.delay.ms != 0 || step.delay.timer_linked {
                    return false;
                }
                let Some(anchor) = step.delay.from.step() else { return false };
                let Some(origin) = self.plan.step(anchor) else { return false };
                origin.leg != step.leg
                    && self.node_complete(anchor)
                    && !armed.iter().any(|prev| {
                        self.plan
                            .step(prev)
                            .is_some_and(|s| s.is_expect() && s.discriminator == step.discriminator)
                    })
            })
    }

    /// Whether every step an item puts on this leg is a `send`.
    ///
    /// Nothing armed consumes a datagram but an expect, so walking past a send
    /// arms nothing and can settle nothing early. It only keeps the search
    /// alive for a relay standing further down the leg.
    fn all_sends(&self, frontier: &[String]) -> bool {
        !frontier.is_empty()
            && frontier.iter().all(|id| self.plan.step(id).is_some_and(|s| !s.is_expect()))
    }

    /// The steps the run is waiting on or about to emit, per leg, in leg order.
    ///
    /// A leg walks its items in order and stops at the first that can still
    /// block it. Three things walk PAST such an item: an item whose whole
    /// frontier on this leg is a tolerated absence, which is what makes
    /// `optional` non-blocking; an item declaring an `overlap` with one already
    /// armed, which is what makes a declared race a race — both steps live at
    /// once, and whichever the wire settles first settles first; and an arrival
    /// another leg has already caused, standing behind sends this leg has not
    /// made yet (`caused_elsewhere`). Only SENDS are walked past that way, and
    /// any number of them: once this leg is waiting on an arrival of its own,
    /// list order is what says which of the two comes first, and it binds.
    pub fn frontier(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (leg, item_indices) in &self.plan.program().by_leg {
            let mut armed: Vec<String> = Vec::new();
            let mut blocked = false;
            let mut awaited = false;
            for &index in item_indices {
                if self.items[index] == ItemStatus::Complete {
                    continue;
                }
                if !self.item_open(index) {
                    break;
                }
                let frontier = self.item_frontier(index, leg);
                // A message item only. An `alt` armed early could COMMIT before
                // the send in front of it goes out, and an `unordered` group
                // arms its whole membership: neither is a relay standing in a
                // queue, and both are authored constructs a capture cannot carry
                // (lint `subset/alt`, `subset/unordered`).
                let is_message = self.plan.program().items[index].kind == ItemKind::Message;
                let caused = !awaited && is_message && self.caused_elsewhere(&frontier, &armed);
                if blocked && !caused && !self.races_with(&frontier, &armed) {
                    // The answers a CANCEL this leg received draws go out at
                    // their own dwell, whatever stands in front (§6.7e).
                    if is_message && self.answers_received_cancel(&frontier) {
                        armed.extend(frontier.iter().cloned());
                        out.extend(frontier);
                        continue;
                    }
                    // An answer to a transaction this leg already opened may be
                    // on the wire now, whatever stands in front of it (§6.7c).
                    if is_message && self.answers_open_transaction(&frontier, &armed) {
                        armed.extend(frontier.iter().cloned());
                        out.extend(frontier);
                        continue;
                    }
                    // A relay can stand behind a RUN of sends, not just one.
                    // Walking past a send ARMS NOTHING — a send is the runner's
                    // own act and never moves earlier — it only keeps the search
                    // alive for a `caused_elsewhere` arrival further down the
                    // leg. The relay's walk still stops dead at an expect of the
                    // leg's own, which is where list order binds for it.
                    if !awaited && self.all_sends(&frontier) {
                        continue;
                    }
                    // A block is never walked past (§6.7b). A step of the leg's
                    // own is: nothing behind it arms but an answer to a
                    // transaction already open, which its place cannot order.
                    if !is_message {
                        break;
                    }
                    awaited = true;
                    continue;
                }
                let all_optional = !frontier.is_empty()
                    && frontier
                        .iter()
                        .all(|id| self.plan.step(id).is_some_and(|s| s.optional_expect()));
                if !caused
                    && frontier.iter().any(|id| {
                        self.plan.step(id).is_some_and(|s| s.is_expect() && !s.optional_expect())
                    })
                {
                    awaited = true;
                }
                armed.extend(frontier.iter().cloned());
                out.extend(frontier);
                if !all_optional {
                    blocked = true;
                }
            }
        }
        out.retain(|id| {
            self.steps.get(id).copied() == Some(StepStatus::Pending)
                && self
                    .plan
                    .step(id)
                    .is_some_and(|s| s.after.iter().all(|node| self.node_complete(node)))
        });
        out.sort();
        out.dedup();
        out
    }

    /// The steps of one item that are live on `leg` right now.
    fn item_frontier(&self, index: usize, leg: &str) -> Vec<String> {
        let item = &self.plan.program().items[index];
        match item.kind {
            ItemKind::Inject => Vec::new(),
            ItemKind::Message => item
                .steps
                .iter()
                .filter(|id| self.plan.step(id).is_some_and(|s| s.leg == leg))
                .cloned()
                .collect(),
            // Every member is armed together; there is no order inside.
            ItemKind::Unordered => item
                .steps
                .iter()
                .filter(|id| self.plan.step(id).is_some_and(|s| s.leg == leg))
                .cloned()
                .collect(),
            ItemKind::Alt => match self.committed.get(&index) {
                // Uncommitted: every branch's FIRST message is armed, and the
                // first to arrive decides.
                None => item
                    .branches
                    .iter()
                    .filter_map(|b| b.steps.first())
                    .filter(|id| self.plan.step(id).is_some_and(|s| s.leg == leg))
                    .cloned()
                    .collect(),
                Some(&branch) => {
                    let steps = &item.branches[branch].steps;
                    let mut out = Vec::new();
                    for id in
                        steps.iter().filter(|id| self.plan.step(id).is_some_and(|s| s.leg == leg))
                    {
                        let Some(step) = self.plan.step(id) else { continue };
                        match self.steps.get(id).copied() {
                            Some(StepStatus::Pending) => {
                                out.push(id.clone());
                                if !step.optional_expect() {
                                    break;
                                }
                            }
                            _ => continue,
                        }
                    }
                    out
                }
            },
        }
    }

    /// The `inject` nodes whose ordering is satisfied and which have not been
    /// handed to the lane's injector yet, as `(node id, action)` — the data the
    /// caller acts on, so the item index never leaves this file.
    pub fn open_injects(&self) -> Vec<(&'p str, &'p str)> {
        self.plan
            .program()
            .items
            .iter()
            .filter(|item| self.items[item.index] == ItemStatus::Pending)
            .filter(|item| !self.injected.contains(&item.index))
            .filter(|item| self.item_open(item.index))
            .filter_map(|item| item.action.as_deref().map(|action| (item.id.as_str(), action)))
            .collect()
    }

    /// Commit an `alt` to the branch a step belongs to. Every step of every
    /// other branch is discarded; the run never backtracks.
    pub fn commit(&mut self, step: &str) {
        let Some(loc) = self.plan.step(step).map(|s| s.loc) else { return };
        let (Some(branch), ItemKind::Alt) = (loc.branch, self.plan.program().items[loc.item].kind)
        else {
            return;
        };
        if self.committed.contains_key(&loc.item) {
            return;
        }
        self.committed.insert(loc.item, branch);
        let item = self.plan.program().items[loc.item].clone();
        for (index, other) in item.branches.iter().enumerate() {
            if index == branch {
                continue;
            }
            for id in &other.steps {
                self.steps.insert(id.clone(), StepStatus::Discarded);
            }
        }
    }

    /// Mark a step done. Returns the `optional` steps its completion released.
    ///
    /// Completing a step inside an uncommitted `alt` commits the alt first: the
    /// commit IS the first discriminating message.
    ///
    /// Only an EXPECT releases a tolerated absence behind it (§6.5: "released
    /// when a later step on the same leg MATCHES first"). A send is emitted, not
    /// matched: the runner controls when it sends, and letting it overtake an
    /// optional expect would discard a message still in flight. The release
    /// stops at the leg's first PENDING step that is not a tolerated absence
    /// (§6.5).
    pub fn complete(&mut self, step: &str) -> Vec<String> {
        self.commit(step);
        let Some(compiled) = self.plan.step(step) else { return Vec::new() };
        let leg = compiled.leg.clone();
        let releases = compiled.is_expect();
        let key = self.leg_key(step);
        self.steps.insert(step.to_string(), StepStatus::Complete);
        self.completed.push(step.to_string());
        let mut released = Vec::new();
        if !releases {
            self.refresh_items();
            return released;
        }
        let mut along: Vec<&CompiledStep> = self
            .plan
            .steps()
            .into_iter()
            .filter(|s| s.leg == leg && self.leg_key(&s.id) < key)
            .filter(|s| self.steps.get(&s.id).copied() == Some(StepStatus::Pending))
            .collect();
        along.sort_by_key(|s| self.leg_key(&s.id));
        let earlier: Vec<String> =
            along.iter().take_while(|s| s.optional_expect()).map(|s| s.id.clone()).collect();
        for id in earlier {
            self.steps.insert(id.clone(), StepStatus::Released);
            self.released.push(id.clone());
            released.push(id);
        }
        self.refresh_items();
        released
    }

    /// Release a tolerated absence: the step did not arrive and the document
    /// said it need not. Refused on anything that is not an `optional` expect —
    /// releasing a step the document requires would turn a failure into a pass.
    pub fn release(&mut self, step: &str) -> bool {
        let releasable = self.plan.step(step).is_some_and(|s| s.optional_expect())
            && self.steps.get(step).copied() == Some(StepStatus::Pending);
        if !releasable {
            return false;
        }
        self.steps.insert(step.to_string(), StepStatus::Released);
        self.released.push(step.to_string());
        self.refresh_items();
        true
    }

    /// Retire a required expect a FINAL on its own transaction has made
    /// unsatisfiable (RFC 3261 §17.1.3): the final was charged on it, and the
    /// leg moves past it. Refused on anything but a pending required expect of
    /// a message item — an `optional` is released, and a block member (`alt`
    /// branch, `unordered` member) keeps the block's own all-or-none rule, as
    /// §6.7c never arms one beside another transaction's answer.
    pub fn retire(&mut self, step: &str) -> bool {
        let retirable = self.plan.step(step).is_some_and(|s| {
            s.is_expect()
                && !s.optional_expect()
                && self.plan.program().items[s.loc.item].kind == ItemKind::Message
        }) && self.steps.get(step).copied() == Some(StepStatus::Pending);
        if !retirable {
            return false;
        }
        self.steps.insert(step.to_string(), StepStatus::Retired);
        self.retired.push(step.to_string());
        self.refresh_items();
        true
    }

    /// The position of a step along its leg: item order first, then position
    /// inside the block.
    fn leg_key(&self, step: &str) -> (usize, usize) {
        match self.plan.step(step) {
            None => (usize::MAX, usize::MAX),
            Some(s) => (s.loc.item, s.loc.within),
        }
    }

    /// Recompute item completion after a step settled.
    fn refresh_items(&mut self) {
        for index in 0..self.items.len() {
            if self.items[index] == ItemStatus::Complete {
                continue;
            }
            let item = &self.plan.program().items[index];
            let done = match item.kind {
                ItemKind::Inject => self.injected.contains(&index),
                ItemKind::Message | ItemKind::Unordered => item
                    .steps
                    .iter()
                    .all(|id| self.steps.get(id).copied().is_some_and(StepStatus::settled)),
                ItemKind::Alt => match self.committed.get(&index) {
                    None => false,
                    Some(&branch) => item.branches[branch]
                        .steps
                        .iter()
                        .all(|id| self.steps.get(id).copied().is_some_and(StepStatus::settled)),
                },
            };
            if done {
                self.items[index] = ItemStatus::Complete;
            }
        }
    }

    /// Whether every top-level item has completed.
    pub fn is_done(&self) -> bool {
        self.items.iter().all(|s| *s == ItemStatus::Complete)
    }

    /// The nodes that never completed — what a `FlowIncomplete` failure names.
    pub fn pending(&self) -> Vec<String> {
        self.plan
            .program()
            .items
            .iter()
            .filter(|item| self.items[item.index] != ItemStatus::Complete)
            .map(|item| item.id.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests;
