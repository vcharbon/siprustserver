//! The **scheduler** (`PCAP2TEST_PIVOT_V3.md` §6, §14 item 2): which steps the
//! run is on right now.
//!
//! Two ordering rules, and no third. Same-leg order is LIST order; cross-leg and
//! cross-call order is `after`. Everything else here is what the three
//! nondeterminism constructs mean at run time:
//!
//! - **`alt`** — every branch's first message is armed at once; the first one to
//!   arrive COMMITS the alt, and the run never backtracks. The other branches'
//!   steps are discarded, which is why nothing outside the alt may reference
//!   them.
//! - **`optional`** — a tolerated absence never blocks its leg. It stays armed
//!   beside the steps behind it, and is RELEASED the moment a later step on the
//!   same leg MATCHES. A send releases nothing: the runner decides when it
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

use crate::plan::Plan;
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

    /// Whether a node id — a step's or a block's — has completed. This is what
    /// `after` waits on.
    pub fn node_complete(&self, node: &str) -> bool {
        if let Some(status) = self.steps.get(node) {
            return matches!(status, StepStatus::Complete | StepStatus::Released);
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
                let caused = !awaited
                    && self.plan.program().items[index].kind == ItemKind::Message
                    && self.caused_elsewhere(&frontier, &armed);
                if blocked && !caused && !self.races_with(&frontier, &armed) {
                    // A relay can stand behind a RUN of sends, not just one.
                    // Walking past a send ARMS NOTHING — a send is the runner's
                    // own act and never moves earlier — it only keeps the search
                    // alive for a `caused_elsewhere` arrival further down the
                    // leg. The walk still stops dead at an expect of the leg's
                    // own, which is where list order binds.
                    if !awaited && self.all_sends(&frontier) {
                        continue;
                    }
                    break;
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
    /// optional expect would discard a message still in flight.
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
        let earlier: Vec<String> = self
            .plan
            .steps()
            .iter()
            .filter(|s| s.leg == leg && s.optional_expect())
            .filter(|s| self.steps.get(&s.id).copied() == Some(StepStatus::Pending))
            .filter(|s| self.leg_key(&s.id) < key)
            .map(|s| s.id.clone())
            .collect();
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

    fn optional(id: &str, leg: &str, status: u16) -> String {
        format!(
            r#"{{"id":"{id}","leg":"{leg}","op":"expect","optional":true,"check":"record","msg":{{"status":{status},"cseq-method":"INVITE"}},"delay":{D}}}"#
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
}
