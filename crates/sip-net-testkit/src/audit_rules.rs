//! The two audit rule interfaces a recording runs: per peer at bind drop, and
//! cross-message over the whole recording at layer close.

use std::collections::HashSet;

use layer_harness::{LaneKey, Stamped};

use crate::contracts::SignalingNetworkEvent;
use crate::types::{all_ua_roles, UaRole};

/// A per-peer rule. Sees the events captured for a single `bind_key` and
/// returns zero or more violation detail strings. `subject` is the SIP role(s)
/// it covers; it runs on a bind only when `subject` intersects the bind's
/// declared roles, and it reports at BIND DROP — the deferred-fail tier, which
/// is what this seam is for. The RFC 3261/3262/3264 suite states every rule as
/// a [`CrossMessageAuditRule`] instead: a per-message obligation is one message
/// at one vantage, which that trait already expresses, and only it can name the
/// offending wire position a scoped waiver attributes on.
pub trait PeerAuditRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn subject(&self) -> HashSet<UaRole> {
        all_ua_roles()
    }
    fn force_advisory(&self) -> bool {
        false
    }
    fn check(&self, events: &[Stamped<SignalingNetworkEvent>], bind_key: &str) -> Vec<String>;
}

/// A cross-message rule operates on the full event log at layer close. Each
/// finding carries an originating `bind_key`. (Port of `CrossMessageAuditRule`.)
pub trait CrossMessageAuditRule: Send + Sync {
    fn name(&self) -> &'static str;
    fn subject(&self) -> HashSet<UaRole> {
        all_ua_roles()
    }
    fn force_advisory(&self) -> bool {
        false
    }
    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)>;

    /// The findings with the 1-based wire-entry index of the OFFENDING message
    /// (into `to_sip_entries(events)`), when the rule can pinpoint it, and the
    /// bind the rule holds RESPONSIBLE — the one that emitted the offending
    /// message, or owed the one never emitted — when it can name it. The
    /// reporting lane is the vantage and is not always the culprit: an
    /// out-of-order request is reported where it was taken, an un-ACKed 2xx
    /// where the ACK was owed. The default derives from [`check`](Self::check)
    /// with neither; a rule that knows overrides THIS instead. Consumers scope
    /// waivers on the index; a finding with neither is unattributable.
    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        self.check(events).into_iter().map(|(b, d)| (b, d, None, None)).collect()
    }

    /// [`check_positioned`](Self::check_positioned) over the projections of
    /// one audit pass: a consumer running several rules over one event list
    /// builds one [`AuditPass`](crate::AuditPass) and hands it to each, and
    /// the findings are those of `check_positioned(pass.events())`. The
    /// default is exactly that call.
    fn check_pass(
        &self,
        pass: &crate::AuditPass<'_>,
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        self.check_positioned(pass.events())
    }
}
