//! The live rows run over a view a consumer took binds out of.

use std::collections::HashSet;
use std::sync::Arc;

use super::*;

/// A row run over a view a consumer took the binds of `scoped_out` out of:
/// recorded parties whose own traffic the view does not hold, so what they
/// sent is never judged at the lane that took it.
pub(super) struct Scoped {
    pub(super) live: Live,
    pub(super) scoped_out: Arc<HashSet<LaneKey>>,
}

impl CrossMessageAuditRule for Scoped {
    fn name(&self) -> &'static str {
        self.live.name()
    }

    fn subject(&self) -> HashSet<UaRole> {
        self.live.subject()
    }

    fn force_advisory(&self) -> bool {
        self.live.advisory
    }

    fn check(&self, events: &[Stamped<SignalingNetworkEvent>]) -> Vec<(LaneKey, String)> {
        self.check_positioned(events).into_iter().map(|(b, d, _, _)| (b, d)).collect()
    }

    fn check_positioned(
        &self,
        events: &[Stamped<SignalingNetworkEvent>],
    ) -> Vec<(LaneKey, String, Option<usize>, Option<LaneKey>)> {
        self.live.found(events, &self.scoped_out)
    }
}

/// [`cross_rules`] over a view the binds of `scoped_out` were taken out of.
pub fn cross_rules_scoped(scoped_out: HashSet<LaneKey>) -> Vec<Arc<dyn CrossMessageAuditRule>> {
    let scoped_out = Arc::new(scoped_out);
    RuleId::ALL
        .iter()
        .flat_map(|&id| rows_of(id))
        .map(|live| {
            let scoped = Scoped { live, scoped_out: scoped_out.clone() };
            Arc::new(scoped) as Arc<dyn CrossMessageAuditRule>
        })
        .collect()
}
