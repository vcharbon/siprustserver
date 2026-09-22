//! The **scheduler** (`PCAP2TEST_PIVOT_V3.md` §6, §14 item 2): which steps the
//! run is on right now.
//!
//! Two ordering rules, and no third. Same-leg order is LIST order; cross-leg and
//! cross-call order is `after`. List order binds within one transaction and
//! between a send and what it provokes; it never binds where nothing could
//! establish it — a relay standing behind a send (§6.7b) and an answer to a
//! transaction this leg has already opened (§6.7c) arm beside the step in
//! front of them. Everything else here is what the three nondeterminism
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

use sip_message::Method;

use crate::plan::{CompiledStep, Discriminator, Plan};
use crate::program::ItemKind;

/// Where one step stands.
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
    /// is the same.
    fn races_with(&self, frontier: &[String], armed: &[String]) -> bool {
        let names =
            |a: &str, b: &str| self.plan.step(a).and_then(|s| s.overlap.as_deref()) == Some(b);
        frontier.iter().any(|id| armed.iter().any(|prev| names(id, prev) || names(prev, id)))
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
    /// consumes a datagram but an expect (`exec.rs`).
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

    /// Whether every step of an item answers a transaction THIS leg has already
    /// opened (§6.7c): an `expect` naming its transaction (`status` plus
    /// `cseq-method`), whose method no expect armed here answers (one
    /// discriminator armed twice would let either step take the other's
    /// datagram), and whose request this leg sent at a completed step.
    /// RFC 3261 §17 orders no two answers on two open transactions, so such a
    /// step arms beside whatever stands in front of it; within one transaction
    /// list order binds.
    fn answers_open_transaction(&self, frontier: &[String], armed: &[String]) -> bool {
        !frontier.is_empty()
            && frontier.iter().all(|id| {
                let Some(step) = self.plan.step(id) else { return false };
                let Discriminator::Response { cseq_method: Some(method), .. } = &step.discriminator
                else {
                    return false;
                };
                let method = Method::from_wire(method);
                step.is_expect()
                    && !armed.iter().any(|prev| {
                        self.plan.step(prev).is_some_and(|s| answers(s, &method).is_some())
                    })
                    && self.transaction_open(step, &method)
            })
    }

    /// Whether the transaction `step` answers is open at `step`'s place in its
    /// leg: the leg's last send of `method` before it has completed, and no
    /// answer to that send stands between the two — pending, which would be
    /// the transaction's earlier turn, or a settled final, which ended it.
    fn transaction_open(&self, step: &CompiledStep, method: &Method) -> bool {
        let Some((opened, behind)) = self.opener_along(step, method) else { return false };
        if self.steps.get(&opened.id).copied() != Some(StepStatus::Complete) {
            return false;
        }
        behind.iter().all(|s| match (answers(s, method), self.steps.get(&s.id)) {
            (Some(_), Some(StepStatus::Pending | StepStatus::Retired)) => false,
            (Some(status), Some(StepStatus::Complete)) => status < 200,
            _ => true,
        })
    }

    /// The send that opened the transaction an expect waits on: the leg's last
    /// `send` of the expect's `cseq-method` standing before it in leg order.
    /// `None` on a step naming no transaction, and on one whose leg sent no
    /// such request before it (a relay of another leg's origination).
    pub fn opening_send(&self, step: &str) -> Option<&'p str> {
        let step = self.plan.step(step)?;
        let Discriminator::Response { cseq_method: Some(method), .. } = &step.discriminator else {
            return None;
        };
        let (opened, _) = self.opener_along(step, &Method::from_wire(method))?;
        Some(opened.id.as_str())
    }

    /// The last send of `method` before `step` on its leg, and every step of
    /// the leg standing between the two, in leg order.
    fn opener_along(
        &self,
        step: &CompiledStep,
        method: &Method,
    ) -> Option<(&'p CompiledStep, Vec<&'p CompiledStep>)> {
        let key = self.leg_key(&step.id);
        let mut along: Vec<&'p CompiledStep> = self
            .plan
            .steps()
            .into_iter()
            .filter(|s| s.leg == step.leg && self.leg_key(&s.id) < key)
            .collect();
        along.sort_by_key(|s| self.leg_key(&s.id));
        let opened = along.iter().rposition(|s| {
            s.is_send()
                && matches!(&s.discriminator, Discriminator::Request { method: m }
                    if Method::from_wire(m) == *method)
        })?;
        let behind = along.split_off(opened + 1);
        Some((along[opened], behind))
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

    /// Whether every item has completed.
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

/// The status an expect waits for on a response to `method`, where it does.
fn answers(step: &CompiledStep, method: &Method) -> Option<u16> {
    match &step.discriminator {
        Discriminator::Response { status, cseq_method: Some(m) }
            if step.is_expect() && Method::from_wire(m) == *method =>
        {
            Some(*status)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(flow: &str) -> Plan {
        let text = format!(
            r#"{{
              "pivot_version": 3,
              "case": {{ "id": "t", "title": "t", "family": "transparent", "variant": "repro",
                        "origin": "authored", "lanes": {{ "upstream-fake": "ok" }} }},
              "identities": [ {{ "name": "caller", "kind": "external-caller", "forms": ["private"] }} ],
              "calls": [ {{ "id": "c1", "caller_leg": "A", "attempts": [
                 {{ "branch": 0, "position": 0, "leg": "B", "callee": {{ "identity": "caller" }} }} ] }} ],
              "endpoints": [ {{ "id": "ep0", "observed": "127.0.0.1:5060", "side": "peer", "binding": "dedicated" }} ],
              "actors": [ {{ "id": "uac1", "kind": "uac", "endpoint": "ep0" }},
                          {{ "id": "uas1", "kind": "uas", "endpoint": "ep0" }} ],
              "legs": [ {{ "id": "A", "actor": "uac1", "dir": "out" }},
                        {{ "id": "B", "actor": "uas1", "dir": "in" }} ],
              "flow": {flow},
              "postconditions": {{ "cdr": {{ "absent": "unit test" }} }},
              "timing": {{ "expect_budget_ms": 1000, "settle_budget_ms": 1000 }}
            }}"#
        );
        let document = pivot_schema::PivotV3::from_json(&text).expect("the fixture parses");
        Plan::compile(document).expect("the fixture compiles")
    }

    const D: &str = r#"{"ms":0,"from":"trigger","compressible":true,"timer_linked":false}"#;

    fn send(id: &str, leg: &str, method: &str) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"send","msg":{{"method":"{method}"}},"delay":{D}}}"#
        )
    }

    fn expect(id: &str, leg: &str, status: u16) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
        )
    }

    fn racing_expect(id: &str, leg: &str, status: u16, with: &str) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","overlap":"{with}","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
        )
    }

    /// An `expect` anchored on a step this leg does not carry: the relay of a
    /// cross-leg origination, whose instant is the anchor plus a latency the
    /// document does not hold.
    fn relayed(id: &str, leg: &str, status: u16, from: &str) -> String {
        let d =
            format!(r#"{{"ms":0,"from":"step:{from}","compressible":true,"timer_linked":false}}"#);
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{d}}}"#
        )
    }

    /// The same anchor with a dwell the document DOES hold: not a relay.
    fn dwelling(id: &str, leg: &str, status: u16, from: &str, ms: u64) -> String {
        let d = format!(
            r#"{{"ms":{ms},"from":"step:{from}","compressible":true,"timer_linked":false}}"#
        );
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{d}}}"#
        )
    }

    fn relayed_request(id: &str, leg: &str, method: &str, from: &str) -> String {
        let d =
            format!(r#"{{"ms":0,"from":"step:{from}","compressible":true,"timer_linked":false}}"#);
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"method":"{method}"}},"delay":{d}}}"#
        )
    }

    /// An expect gated on a response to a transaction the leg names.
    fn answer(id: &str, leg: &str, status: u16, method: &str) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status},"cseq-method":"{method}"}},"delay":{D}}}"#
        )
    }

    /// An expect gated on a status alone, naming no transaction.
    fn status_only(id: &str, leg: &str, status: u16) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"status":{status}}},"delay":{D}}}"#
        )
    }

    fn request_expect(id: &str, leg: &str, method: &str) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","check":"record","msg":{{"method":"{method}"}},"delay":{D}}}"#
        )
    }

    fn optional(id: &str, leg: &str, status: u16) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","optional":true,"check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
        )
    }

    /// An `optional` expect of a request relayed from `from` on another leg.
    fn optional_relayed_request(id: &str, leg: &str, method: &str, from: &str) -> String {
        let d =
            format!(r#"{{"ms":0,"from":"step:{from}","compressible":true,"timer_linked":false}}"#);
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","optional":true,"check":"record","msg":{{"method":"{method}"}},"delay":{d}}}"#
        )
    }

    /// An `optional` expect gated on a response to a transaction the leg names.
    fn optional_answer(id: &str, leg: &str, status: u16, method: &str) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","optional":true,"check":"record","msg":{{"status":{status},"cseq-method":"{method}"}},"delay":{D}}}"#
        )
    }

    /// A declared race arms both steps at once, so the leg is not committed to
    /// the order the capture happened to see.
    #[test]
    fn a_declared_race_arms_beside_the_step_it_names() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "BYE"),
            racing_expect("s3", "A", 200, "s2")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2", "s3"], "the race is armed together");
        // Either side may settle first: here the arrival beats the send.
        cursor.complete("s3");
        assert_eq!(cursor.frontier(), ["s2"]);
    }

    /// The race walks past ONE neighbour, not the whole leg.
    #[test]
    fn a_declared_race_does_not_arm_the_step_behind_it() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "BYE"),
            racing_expect("s2", "A", 200, "s1"),
            expect("s3", "A", 486)
        ));
        let cursor = Cursor::new(&p);
        assert_eq!(cursor.frontier(), ["s1", "s2"], "s3 still waits its turn");
    }

    #[test]
    fn a_leg_runs_its_items_in_list_order_and_legs_run_concurrently() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "INVITE"),
            expect("s2", "A", 100),
            send("s3", "B", "INVITE"),
            expect("s4", "B", 200)
        ));
        let mut cursor = Cursor::new(&p);
        assert_eq!(cursor.frontier(), ["s1", "s3"], "one head per leg");
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2", "s3"]);
        cursor.complete("s3");
        cursor.complete("s2");
        cursor.complete("s4");
        assert!(cursor.is_done());
        assert_eq!(cursor.pending(), Vec::<String>::new());
    }

    #[test]
    fn after_holds_a_step_until_the_node_it_names_completes() {
        let mut b = format!(
            r#"{{"id":"s3","leg":"B","op":"send","after":["s2"],"msg":{{"method":"INVITE"}},"delay":{D}}}"#
        );
        b = format!("[{},{},{}]", send("s1", "A", "INVITE"), expect("s2", "A", 100), b);
        let p = plan(&b);
        let mut cursor = Cursor::new(&p);
        assert_eq!(cursor.frontier(), ["s1"], "s3 waits on s2");
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3"]);
    }

    #[test]
    fn an_alt_arms_every_branch_and_commits_on_the_first_arrival() {
        let alt = format!(
            r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{},{}]}},
                 {{"name":"cancelled","steps":[{}]}}]}}"#,
            expect("s2", "A", 200),
            send("s3", "A", "ACK"),
            expect("s4", "A", 487)
        );
        let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), alt));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2", "s4"], "both branch heads are armed");
        cursor.complete("s4");
        assert_eq!(cursor.committed_branch("a1"), Some("cancelled"));
        assert_eq!(cursor.status("s2"), Some(StepStatus::Discarded));
        assert_eq!(cursor.status("s3"), Some(StepStatus::Discarded));
        assert!(cursor.is_done(), "the committed branch is complete");
    }

    #[test]
    fn a_committed_alt_runs_its_own_branch_in_order_and_never_backtracks() {
        let alt = format!(
            r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{},{}]}},
                 {{"name":"cancelled","steps":[{}]}}]}}"#,
            expect("s2", "A", 200),
            send("s3", "A", "ACK"),
            expect("s4", "A", 487)
        );
        let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), alt));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.committed_branch("a1"), Some("answered"));
        assert_eq!(cursor.frontier(), ["s3"], "the other branch is gone");
        cursor.complete("s3");
        assert!(cursor.is_done());
    }

    #[test]
    fn an_unordered_group_arms_every_member_and_completes_on_the_last() {
        let bye = format!(
            r#"{{"id":"s3","leg":"A","op":"expect","check":"record","msg":{{"method":"BYE"}},"delay":{D}}}"#
        );
        let group = format!(
            r#"{{"id":"u1","op":"unordered","steps":[{},{}]}}"#,
            expect("s2", "A", 481),
            bye
        );
        let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), group));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2", "s3"], "order-free: both armed");
        cursor.complete("s3");
        assert_eq!(cursor.frontier(), ["s2"], "the group is not done until all arrive");
        assert!(!cursor.is_done());
        cursor.complete("s2");
        assert!(cursor.is_done());
    }

    #[test]
    fn an_optional_expect_never_blocks_its_leg_and_is_released_by_the_step_that_overtakes_it() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "INVITE"),
            optional("s2", "A", 180),
            expect("s3", "A", 200)
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2", "s3"], "the tolerated absence does not block");
        let released = cursor.complete("s3");
        assert_eq!(released, ["s2"]);
        assert_eq!(cursor.status("s2"), Some(StepStatus::Released));
        assert!(cursor.is_done());
        assert_eq!(cursor.released(), ["s2"]);
    }

    #[test]
    fn an_optional_expect_that_does_arrive_completes_rather_than_releasing() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "INVITE"),
            optional("s2", "A", 180),
            expect("s3", "A", 200)
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert!(cursor.complete("s2").is_empty());
        assert_eq!(cursor.status("s2"), Some(StepStatus::Complete));
        assert_eq!(cursor.frontier(), ["s3"]);
    }

    #[test]
    fn a_send_does_not_release_a_tolerated_absence_it_overtakes() {
        // The runner controls when it sends, so a send overtaking an optional
        // expect says nothing about a message still in flight toward it.
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "INVITE"),
            optional("s2", "A", 180),
            send("s3", "A", "CANCEL")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2", "s3"]);
        assert!(cursor.complete("s3").is_empty(), "a send releases nothing");
        assert_eq!(cursor.status("s2"), Some(StepStatus::Pending));
        assert_eq!(cursor.frontier(), ["s2"], "the tolerated absence is still armed");
    }

    #[test]
    fn only_a_tolerated_absence_may_be_released() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "INVITE"),
            optional("s2", "A", 180),
            expect("s3", "A", 200)
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert!(!cursor.release("s3"), "a required expect is never released");
        assert!(!cursor.release("s1"), "a settled step is never released");
        assert!(cursor.release("s2"));
        assert_eq!(cursor.status("s2"), Some(StepStatus::Released));
        assert!(!cursor.release("s2"), "releasing twice changes nothing");
        assert_eq!(cursor.released(), ["s2"]);
    }

    #[test]
    fn an_incomplete_run_names_the_nodes_that_never_completed() {
        let p = plan(&format!("[{},{}]", send("s1", "A", "INVITE"), expect("s2", "A", 200)));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert!(!cursor.is_done());
        assert_eq!(cursor.pending(), ["s2"]);
    }

    /// A relay whose cause has already fired may be on the wire NOW, so a send
    /// this leg has not made yet cannot be what orders it.
    #[test]
    fn an_arrival_another_leg_has_caused_arms_behind_a_send() {
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "A", "INVITE"),
            send("s3", "B", "INVITE"),
            relayed("s4", "B", 100, "s2"),
            relayed_request("s5", "B", "BYE", "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(
            cursor.frontier(),
            ["s3", "s4", "s5"],
            "both relays are live beside the send this leg has not made yet"
        );
        // The relay beats the send: the leg takes it and its own order stands.
        cursor.complete("s5");
        assert_eq!(cursor.frontier(), ["s3", "s4"]);
    }

    /// A relay stands behind however many sends the leg has not made yet: the
    /// walk crosses a RUN of them, not exactly one. Real-clock replay puts the
    /// arrival one step earlier in the list than virtual time does, and the
    /// document is the same document (issue 195).
    #[test]
    fn an_arrival_another_leg_has_caused_arms_behind_a_run_of_sends() {
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "ACK"),
            send("s3", "B", "INVITE"),
            relayed("s4", "B", 100, "s1"),
            relayed_request("s5", "B", "BYE", "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(
            cursor.frontier(),
            ["s2", "s4", "s5"],
            "the relays live beside the FIRST send; s3 is walked past, not armed"
        );
        // The relay beats both sends: the leg takes it and its own order stands.
        cursor.complete("s5");
        assert_eq!(cursor.frontier(), ["s2", "s4"]);
    }

    /// Walking past a send never emits it early. The runner decides when it
    /// sends, and only the leg's own head is ever armed to do so.
    #[test]
    fn a_send_walked_past_is_not_armed_early() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "ACK"),
            send("s3", "B", "INVITE"),
            relayed_request("s4", "B", "BYE", "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        let frontier = cursor.frontier();
        assert!(frontier.contains(&"s4".to_string()), "the relay armed");
        assert!(!frontier.contains(&"s3".to_string()), "the send behind s2 did not");
        // s3 arms only once s2 has actually gone out.
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3", "s4"]);
    }

    /// The run of sends is crossed, an expect of the leg's own still is not:
    /// a walk that passed sends does not thereby earn the right to pass an
    /// arrival list order binds.
    #[test]
    fn a_run_of_sends_does_not_carry_the_walk_past_the_legs_own_expect() {
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "ACK"),
            send("s3", "B", "INVITE"),
            expect("s4", "B", 180),
            relayed_request("s5", "B", "BYE", "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2"], "s4 anchors on the trigger, so s5 waits");
    }

    /// The walk is a walk, not a jump: a step anchored on THIS leg is the
    /// document's own order and stops it, relay or no relay behind it.
    #[test]
    fn a_step_anchored_on_its_own_leg_stops_the_walk() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "INVITE"),
            expect("s3", "B", 100),
            relayed_request("s4", "B", "BYE", "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2"], "s3 anchors on the trigger, so s4 waits");
    }

    /// The cause is the gate. An anchor that has not fired states an arrival
    /// nothing has provoked, and list order is all there is to go on.
    #[test]
    fn an_arrival_whose_cause_has_not_fired_waits_its_turn() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "INVITE"),
            relayed_request("s3", "B", "BYE", "s1")
        ));
        let cursor = Cursor::new(&p);
        assert_eq!(cursor.frontier(), ["s1", "s2"], "s3 has nothing to wait for yet");
    }

    /// Only a SEND is walked past. Once the leg is waiting on an arrival of its
    /// own, which of the two comes first is what list order says.
    #[test]
    fn an_arrival_another_leg_has_caused_waits_behind_an_expect() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "BYE"),
            expect("s2", "B", 180),
            relayed_request("s3", "B", "BYE", "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2"], "the leg's own expect binds the order");
    }

    /// A relay arms early only where the leg can TELL IT APART: sharing a
    /// discriminator with one already armed would let the earlier step's
    /// datagram settle the later one.
    #[test]
    fn an_arrival_sharing_a_discriminator_with_one_armed_does_not_arm() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "INVITE"),
            relayed("s3", "B", 200, "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2", "s3"], "nothing shares its discriminator");

        let q = plan(&format!(
            "[{},{},{},{}]",
            send("t1", "A", "BYE"),
            send("t2", "B", "INVITE"),
            relayed("t3", "B", 200, "t1"),
            relayed("t4", "B", 200, "t1")
        ));
        let mut other = Cursor::new(&q);
        other.complete("t1");
        assert_eq!(
            other.frontier(),
            ["t2", "t3"],
            "t4 would take t3's datagram, so list order keeps it behind"
        );
    }

    /// A cross-leg anchor carrying a REAL dwell is a duration the DOCUMENT
    /// holds, and §6.7a's closing rule stands for it: its order is its own.
    /// Only the synthetic zero of a relay walks past a send.
    #[test]
    fn a_cross_leg_dwell_the_document_holds_waits_its_turn() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "INVITE"),
            dwelling("s3", "B", 200, "s1", 700)
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2"], "700 ms is the document's own dwell");
    }

    /// An `alt` is never walked past. Armed early it could COMMIT before the
    /// send in front of it goes out, discarding the branch that send belongs to.
    #[test]
    fn an_alt_behind_a_send_is_never_armed_early() {
        let alt = format!(
            r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"released","steps":[{}]}},
                 {{"name":"answered","steps":[{}]}}]}}"#,
            relayed_request("s3", "B", "BYE", "s1"),
            relayed("s4", "B", 200, "s1")
        );
        let p =
            plan(&format!("[{},{},{}]", send("s1", "A", "BYE"), send("s2", "B", "INVITE"), alt));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2"], "the alt waits for the send in front of it");
    }

    /// Two answers this leg is owed on two transactions it opened carry no
    /// order between them (RFC 3261 §17): the BYE's 200 arms beside the
    /// INVITE's 487 the document lists first, and whichever lands first is
    /// taken first.
    #[test]
    fn an_answer_to_a_transaction_this_leg_opened_arms_beside_another_transaction_s_answer() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "BYE"),
            answer("s3", "A", 487, "INVITE"),
            answer("s4", "A", 200, "BYE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3", "s4"], "both transactions are open: both armed");
        cursor.complete("s4");
        assert_eq!(cursor.frontier(), ["s3"], "the 487 is still owed");
        cursor.complete("s3");
        assert!(cursor.is_done());
    }

    /// The answer may be on the wire from the moment its request went out, so
    /// a send this leg has not made yet cannot be what orders it — and walking
    /// past that send never emits it early.
    #[test]
    fn an_answer_to_an_open_transaction_arms_behind_a_send() {
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "BYE"),
            answer("s3", "A", 487, "INVITE"),
            send("s4", "A", "ACK"),
            answer("s5", "A", 200, "BYE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3", "s5"], "the ACK behind the 487 is walked past");
        cursor.complete("s5");
        cursor.complete("s3");
        assert_eq!(cursor.frontier(), ["s4"], "the ACK arms only once its turn comes");
    }

    /// The request is the gate: an answer to a request this leg has not sent
    /// yet is an arrival nothing has provoked, and list order is all there is.
    #[test]
    fn an_answer_whose_request_has_not_gone_out_waits_its_turn() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "INVITE"),
            answer("s2", "A", 180, "INVITE"),
            send("s3", "A", "BYE"),
            answer("s4", "A", 200, "BYE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2"], "the BYE has not gone out");
    }

    /// Within ONE transaction list order binds: a provisional before its final.
    /// Beside an answer on ANOTHER transaction, only the transaction's first
    /// pending answer arms, and its final stays behind it.
    #[test]
    fn answers_on_one_transaction_keep_list_order() {
        let p = plan(&format!(
            "[{},{},{}]",
            send("s1", "A", "INVITE"),
            answer("s2", "A", 180, "INVITE"),
            answer("s3", "A", 200, "INVITE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert_eq!(cursor.frontier(), ["s2"], "the final waits behind the provisional");

        let q = plan(&format!(
            "[{},{},{},{},{}]",
            send("t1", "A", "INVITE"),
            send("t2", "A", "PRACK"),
            answer("t3", "A", 200, "PRACK"),
            answer("t4", "A", 180, "INVITE"),
            answer("t5", "A", 200, "INVITE")
        ));
        let mut other = Cursor::new(&q);
        other.complete("t1");
        other.complete("t2");
        assert_eq!(
            other.frontier(),
            ["t3", "t4"],
            "the INVITE's next provisional arms beside the PRACK's answer; its final does not"
        );
    }

    /// An expect gated on a status alone names no transaction, so nothing says
    /// which request it answers, and it waits its turn.
    #[test]
    fn an_answer_naming_no_transaction_waits_its_turn() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "BYE"),
            answer("s3", "A", 487, "INVITE"),
            status_only("s4", "A", 200)
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3"]);
    }

    /// A final already taken closed its transaction (RFC 3261 §17.1): a later
    /// answer of the same method — a fork's second 2xx — is not an open
    /// transaction's and keeps its place in the list.
    #[test]
    fn a_transaction_a_final_already_answered_opens_nothing() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "INVITE"),
            answer("s2", "A", 200, "INVITE"),
            request_expect("s3", "A", "BYE"),
            answer("s4", "A", 200, "INVITE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3"], "the second 2xx waits behind the BYE");
    }

    /// A final on a transaction retires the required expect it was charged on:
    /// the step settles, what stands behind it arms, and an `after` or a dwell
    /// anchored on it is satisfied.
    #[test]
    fn a_retired_expect_settles_and_what_stands_behind_it_arms() {
        let p = plan(&format!(
            "[{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "PRACK"),
            answer("s3", "A", 481, "PRACK"),
            send("s4", "A", "ACK")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3"]);
        assert!(cursor.retire("s3"), "a pending required expect of a message item retires");
        assert_eq!(cursor.status("s3"), Some(StepStatus::Retired));
        assert!(cursor.node_complete("s3"), "an anchor on a retired step is satisfied");
        assert_eq!(cursor.retired(), ["s3"]);
        assert_eq!(cursor.frontier(), ["s4"], "the leg moves past it");
        assert!(!cursor.retire("s3"), "retired once");
    }

    /// Retirement is refused on everything that is not a pending required
    /// expect of a message item: a send, a completed step, an `optional` (which
    /// is released instead), an `alt` branch and an `unordered` member.
    #[test]
    fn retirement_is_refused_outside_a_pending_required_message_expect() {
        let alt = format!(
            r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{}]}},
                 {{"name":"busy","steps":[{}]}}]}}"#,
            expect("s3", "A", 200),
            expect("s4", "A", 486)
        );
        let group = format!(
            r#"{{"id":"u1","op":"unordered","steps":[{},{}]}}"#,
            answer("s6", "A", 200, "BYE"),
            answer("s7", "A", 487, "INVITE")
        );
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            optional("s2", "A", 183),
            alt,
            send("s5", "A", "BYE"),
            group
        ));
        let mut cursor = Cursor::new(&p);
        assert!(!cursor.retire("s1"), "a send");
        cursor.complete("s1");
        assert!(!cursor.retire("s1"), "a completed step");
        assert!(!cursor.retire("s2"), "an optional is released, never retired");
        assert!(cursor.release("s2"));
        assert!(!cursor.retire("s3") && !cursor.retire("s4"), "an alt branch");
        cursor.complete("s3");
        cursor.complete("s5");
        assert_eq!(cursor.frontier(), ["s6", "s7"]);
        assert!(!cursor.retire("s6") && !cursor.retire("s7"), "an unordered member");
        assert!(cursor.retired().is_empty());
    }

    /// A retired final CLOSES its transaction for §6.7c: a later answer of the
    /// same method behind it is another transaction's, not yet opened, and is
    /// not walked to.
    #[test]
    fn a_required_expect_behind_a_retired_final_of_its_transaction_is_not_walked_to() {
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            answer("s2", "A", 486, "INVITE"),
            send("s3", "A", "ACK"),
            request_expect("s4", "A", "BYE"),
            answer("s5", "A", 200, "INVITE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        assert!(cursor.retire("s2"));
        cursor.complete("s3");
        assert_eq!(cursor.frontier(), ["s4"], "the 200 is not the retired transaction's answer");
    }

    /// The send that opened the transaction an expect waits on: the leg's last
    /// send of that method before it, whatever stands between; none for a
    /// status-only expect or a leg that never sent the method.
    #[test]
    fn the_opening_send_is_the_last_send_of_the_method_before_the_expect() {
        let p = plan(&format!(
            "[{},{},{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "PRACK"),
            answer("s3", "A", 200, "PRACK"),
            send("s4", "A", "PRACK"),
            answer("s5", "A", 481, "PRACK"),
            answer("s6", "A", 600, "INVITE"),
            status_only("s7", "A", 200)
        ));
        let cursor = Cursor::new(&p);
        assert_eq!(cursor.opening_send("s3"), Some("s2"));
        assert_eq!(cursor.opening_send("s5"), Some("s4"));
        assert_eq!(cursor.opening_send("s6"), Some("s1"));
        assert_eq!(cursor.opening_send("s7"), None, "a status alone names no transaction");
        assert_eq!(cursor.opening_send("s1"), None, "a send waits on nothing");
        let relay =
            plan(&format!("[{},{}]", send("s1", "B", "INVITE"), relayed("s2", "A", 183, "s1")));
        assert_eq!(Cursor::new(&relay).opening_send("s2"), None, "this leg sent no INVITE");
    }

    /// An answer armed beside a REQUIRED expect still pending (§6.7c) releases
    /// no tolerated absence standing between the two: their order against the
    /// answer is unknown, and the INVITE's 180 behind its pending 183 may still
    /// come.
    #[test]
    fn an_answer_armed_beside_a_pending_expect_releases_no_optional_behind_it() {
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "INFO"),
            answer("s3", "A", 183, "INVITE"),
            optional("s4", "A", 180),
            answer("s5", "A", 200, "INFO")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3", "s5"], "the INFO's answer arms beside the 183");
        assert!(cursor.complete("s5").is_empty(), "nothing released past the pending 183");
        assert_eq!(cursor.status("s4"), Some(StepStatus::Pending));
        assert!(cursor.complete("s3").is_empty());
        assert_eq!(cursor.frontier(), ["s4"], "the tolerated 180 is armed in its turn");
        cursor.complete("s4");
        assert_eq!(cursor.status("s4"), Some(StepStatus::Complete));
        assert!(cursor.is_done());
    }

    /// A relay armed early behind a send this leg has not made (§6.7b) releases
    /// no tolerated absence standing behind that send: the send has not gone
    /// out, so nothing says the optional's message will not still come.
    /// Optionals standing before the pending send are released as ever.
    #[test]
    fn a_relay_armed_past_a_pending_send_releases_no_optional_behind_it() {
        let p = plan(&format!(
            "[{},{},{},{},{},{}]",
            send("s1", "A", "BYE"),
            send("s2", "B", "INVITE"),
            optional("s3", "B", 100),
            send("s4", "B", "ACK"),
            optional_relayed_request("s5", "B", "INFO", "s1"),
            relayed_request("s6", "B", "BYE", "s1")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3", "s4", "s5", "s6"]);
        assert_eq!(cursor.complete("s6"), ["s3"], "the optional before the send is released");
        assert_eq!(cursor.status("s5"), Some(StepStatus::Pending), "the one behind it is not");
        cursor.complete("s4");
        assert_eq!(cursor.frontier(), ["s5"]);
    }

    /// An armed answer of the same METHOD refuses the candidate even where it
    /// waits on an older transaction: two steps with one discriminator armed
    /// together would take each other's datagram.
    #[test]
    fn an_answer_does_not_arm_beside_an_armed_answer_of_the_same_method() {
        let p = plan(&format!(
            "[{},{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "PRACK"),
            optional_answer("s3", "A", 200, "PRACK"),
            send("s4", "A", "PRACK"),
            request_expect("s5", "A", "INFO"),
            answer("s6", "A", 200, "PRACK")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        cursor.complete("s4");
        assert_eq!(cursor.frontier(), ["s3", "s5"], "s6 waits: s3 would take its 200");
    }

    /// A block standing between the leg's blocking step and the candidate stops
    /// the walk: an `unordered` group is never walked past.
    #[test]
    fn an_unordered_group_in_front_stops_the_walk_to_an_open_transaction_s_answer() {
        let group = format!(
            r#"{{"id":"u1","op":"unordered","steps":[{},{}]}}"#,
            request_expect("s4", "A", "INFO"),
            request_expect("s5", "A", "UPDATE")
        );
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "BYE"),
            answer("s3", "A", 180, "INVITE"),
            group,
            answer("s6", "A", 200, "BYE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3"], "the group is not walked past");
    }

    /// A block that IS the leg's blocking item lets an answer to another open
    /// transaction arm beside it: the branch heads are armed already, and a
    /// candidate on their own transaction is refused.
    #[test]
    fn an_answer_arms_beside_an_armed_alt() {
        let alt = format!(
            r#"{{"id":"a1","op":"alt","branches":[
                 {{"name":"answered","steps":[{}]}},
                 {{"name":"busy","steps":[{}]}}]}}"#,
            answer("s3", "A", 200, "INVITE"),
            answer("s4", "A", 486, "INVITE")
        );
        let p = plan(&format!(
            "[{},{},{},{},{}]",
            send("s1", "A", "INVITE"),
            send("s2", "A", "BYE"),
            alt,
            answer("s5", "A", 200, "BYE"),
            answer("s6", "A", 487, "INVITE")
        ));
        let mut cursor = Cursor::new(&p);
        cursor.complete("s1");
        cursor.complete("s2");
        assert_eq!(cursor.frontier(), ["s3", "s4", "s5"], "the INVITE's 487 is refused");
    }
}
