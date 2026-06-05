//! The `CallScenario` / safe-point DSL (ADR-0013). A v1 scenario is a callflow
//! driven to a quiescent **safe-point**, where a failover is (optionally)
//! injected, after which the backup processes one [`Event`]; every scenario then
//! drives the call to **full termination** so the universal teardown sweep
//! (`oracle::TeardownSweep`) holds for every cell.
//!
//! v1 keeps the step vocabulary small and the safe-point implicit (one per
//! scenario, immediately after the dialog reaches [`Cell::state`]). The `Event`
//! axis is the behavioral category the backup must handle. New callflows extend
//! [`Event`] / the runner; the declarative-step-list generalisation (multiple
//! explicit safe-points per flow) is the documented next step.

use sip_message::generators::InDialogMethod;

/// Who originates an in-dialog request — the caller leg (alice) or the callee
/// leg (bob). The b2bua relays leg-to-leg, so direction is behaviorally a
/// modifier the matrix folds into the seeded pick for the generic category.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Party {
    Caller,
    Callee,
}

impl Party {
    pub fn label(self) -> &'static str {
        match self {
            Party::Caller => "alice",
            Party::Callee => "bob",
        }
    }
}

/// The dialog state at the safe-point where the failover is injected. v1 covers
/// the three *quiescent* states (a safe-point demands quiescence, so
/// re-negotiating-in-flight / terminating-in-flight are v2-disruptive).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DialogState {
    /// INVITE sent, 18x received, no final response — CANCEL territory.
    Early,
    /// 200 OK sent, ACK pending (the "200/ACK on backup" window).
    ConfirmedPreAck,
    /// Post-ACK, stable confirmed dialog.
    Established,
}

impl DialogState {
    pub fn label(self) -> &'static str {
        match self {
            DialogState::Early => "early",
            DialogState::ConfirmedPreAck => "confirmed_pre_ack",
            DialogState::Established => "established",
        }
    }
}

/// The behavioral category of in-dialog event the backup processes after the
/// failover (axis B). Direction/method for the generic category is a seeded
/// pick, not a full permutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// Terminating: a BYE from `Party` → 200, call torn down on the backup.
    Bye(Party),
    /// Terminating: CANCEL of an early INVITE (caller) → 487 Request Terminated.
    Cancel,
    /// Non-terminating in-dialog request (re-INVITE / UPDATE / INFO / OPTIONS
    /// ping), behaviorally interchangeable. Call stays Active → timers re-arm.
    Generic { method: InDialogMethod, from: Party },
    /// The AS's own keepalive OPTIONS probe — refreshes dead-peer detection AND
    /// the call-limiter hold on the owner.
    Keepalive,
    /// No event routed to the backup; pure re-hydration on reboot.
    Nothing,
}

impl Event {
    /// A short, file-name-safe label for the generated test name.
    pub fn label(self) -> String {
        match self {
            Event::Bye(p) => format!("bye_{}", p.label()),
            Event::Cancel => "cancel".into(),
            Event::Generic { method, from } => {
                format!("generic_{}_{}", method_label(method), from.label())
            }
            Event::Keepalive => "keepalive".into(),
            Event::Nothing => "nothing".into(),
        }
    }

    /// Whether the event itself terminates the call (so the runner does not
    /// append a final BYE).
    pub fn is_terminating(self) -> bool {
        matches!(self, Event::Bye(_) | Event::Cancel)
    }
}

pub(crate) fn method_label(m: InDialogMethod) -> &'static str {
    match m {
        InDialogMethod::Invite => "reinvite",
        InDialogMethod::Update => "update",
        InDialogMethod::Info => "info",
        InDialogMethod::Options => "options",
        InDialogMethod::Bye => "bye",
        _ => "indialog",
    }
}

/// The failure injected at the safe-point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Abrupt process death: abort tasks, wipe store, streams close.
    Kill,
    /// Graceful SIGTERM: the primary drains (decode_stickiness grace window)
    /// before the proxy routes to the backup.
    Drain,
}

impl Fault {
    pub fn label(self) -> &'static str {
        match self {
            Fault::Kill => "kill",
            Fault::Drain => "drain",
        }
    }
}

/// What happens after the failover (axis D).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recovery {
    /// Primary never returns; the backup serves the rest of the call.
    StayDead,
    /// Primary reboots with no traffic routed while it was dead → clean reclaim
    /// (pairs with `Event::Nothing`).
    RebootNoTraffic,
    /// Primary reboots after the backup took the call over → reclaim + handback.
    RebootAfterTakeover,
}

impl Recovery {
    pub fn label(self) -> &'static str {
        match self {
            Recovery::StayDead => "stay_dead",
            Recovery::RebootNoTraffic => "reboot_no_traffic",
            Recovery::RebootAfterTakeover => "reboot_after_takeover",
        }
    }
}

/// A full matrix cell: the callflow shape + the failover plan + a seed (for the
/// generic category's `{method × direction}` pick — deterministic per cell).
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub state: DialogState,
    pub event: Event,
    pub fault: Fault,
    pub recovery: Recovery,
    pub seed: u64,
}

impl Cell {
    /// The generated `#[tokio::test]` name: `<state>__<event>__<fault>__<recovery>`.
    pub fn name(&self) -> String {
        format!(
            "{}__{}__{}__{}",
            self.state.label(),
            self.event.label(),
            self.fault.label(),
            self.recovery.label(),
        )
    }
}
