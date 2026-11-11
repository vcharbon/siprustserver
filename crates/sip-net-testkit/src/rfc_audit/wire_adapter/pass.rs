//! One audit pass over a recorded event list: the projections every live row
//! reads, each built at most once however many rows the pass runs.

use std::borrow::Cow;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};

use layer_harness::{LaneKey, Stamped};
use rfc_rules::Observation;

use crate::contracts::{audit_visible_event, SignalingNetworkEvent};
use crate::report::wire_positions_by_stamp;
use crate::rfc_audit::dropped::Dropped;
use crate::rfc_audit::recorded::Recorded;
use crate::rfc_audit::relay_lanes::relay_lanes;
use crate::types::UaRole;

use super::party_view;
use super::view::{bind_views, observation, BindView};

/// The shared projection of one recorded event list. A consumer that runs
/// several cross-message rules over one list builds one pass and hands it to
/// each ([`CrossMessageAuditRule::check_pass`](crate::CrossMessageAuditRule::check_pass));
/// every projection is built on first read and read unchanged by every later
/// rule, so a rule's findings over a shared pass are its findings over a
/// pass of its own.
pub struct AuditPass<'e> {
    all: &'e [Stamped<SignalingNetworkEvent>],
    visible: Cow<'e, [Stamped<SignalingNetworkEvent>]>,
    positions: OnceCell<HashMap<u64, usize>>,
    views: OnceCell<Vec<(BindView, Observation)>>,
    party_views: OnceCell<Vec<Option<(BindView, Observation)>>>,
    recorded: OnceCell<Recorded>,
    dropped: OnceCell<Dropped>,
    relay_lanes: OnceCell<HashSet<LaneKey>>,
    proxy_only_lanes: OnceCell<HashSet<LaneKey>>,
}

impl<'e> AuditPass<'e> {
    /// The pass over `events`. The rules judge the audit-visible events; the
    /// arrivals the loss model dropped stay readable as evidence only.
    pub fn new(events: &'e [Stamped<SignalingNetworkEvent>]) -> AuditPass<'e> {
        let visible = if events.iter().all(|s| audit_visible_event(&s.event)) {
            Cow::Borrowed(events)
        } else {
            Cow::Owned(events.iter().filter(|s| audit_visible_event(&s.event)).cloned().collect())
        };
        AuditPass {
            all: events,
            visible,
            positions: OnceCell::new(),
            views: OnceCell::new(),
            party_views: OnceCell::new(),
            recorded: OnceCell::new(),
            dropped: OnceCell::new(),
            relay_lanes: OnceCell::new(),
            proxy_only_lanes: OnceCell::new(),
        }
    }

    /// The event list the pass was built over.
    pub fn events(&self) -> &'e [Stamped<SignalingNetworkEvent>] {
        self.all
    }

    /// The 1-based wire-entry position of each audit-visible event's stamp.
    pub(super) fn positions(&self) -> &HashMap<u64, usize> {
        self.positions.get_or_init(|| {
            built("positions");
            wire_positions_by_stamp(&self.visible)
        })
    }

    /// Every bind's view of the audit-visible events, in first-sighting
    /// order, with its closed observation.
    pub(super) fn views(&self) -> &[(BindView, Observation)] {
        self.views.get_or_init(|| {
            built("views");
            bind_views(&self.visible)
                .into_iter()
                .map(|view| {
                    let obs = observation(&view.msgs);
                    (view, obs)
                })
                .collect()
        })
    }

    /// Every bind's view, merged with the views of the binds one party holds
    /// with it ([`rfc_rules::rules::correlation::Parties`]): a party that
    /// sends from one socket and listens on another is judged on its whole
    /// stream. The view of a bind no other shares a party with is its own.
    pub(super) fn party_views(&self) -> Vec<(&BindView, &Observation)> {
        let merged = self.party_views.get_or_init(|| {
            built("party_views");
            let views = self.views();
            let own: Vec<&BindView> = views.iter().map(|(v, _)| v).collect();
            let joined = party_view::joined(&own);
            own.iter()
                .map(|v| {
                    let members = joined.get(&v.bind)?;
                    let view = party_view::merged(v, members, &own);
                    let obs = observation(&view.msgs);
                    Some((view, obs))
                })
                .collect()
        });
        self.views()
            .iter()
            .zip(merged)
            .map(|(own, merged)| match merged {
                Some((v, o)) => (v, o),
                None => (&own.0, &own.1),
            })
            .collect()
    }

    /// The parties the audit-visible events hold a lane for.
    pub(super) fn recorded(&self) -> &Recorded {
        self.recorded.get_or_init(|| {
            built("recorded");
            Recorded::of(&self.visible, &HashSet::new())
        })
    }

    /// The arrivals the loss model dropped, over every event.
    pub(super) fn dropped(&self) -> &Dropped {
        self.dropped.get_or_init(|| {
            built("dropped");
            Dropped::of(self.all)
        })
    }

    /// The lanes that relay rather than author.
    pub(super) fn relay_lanes(&self) -> &HashSet<LaneKey> {
        self.relay_lanes.get_or_init(|| {
            built("relay_lanes");
            relay_lanes(&self.visible)
        })
    }

    /// The lanes declared `{Proxy}` only.
    pub(super) fn proxy_only_lanes(&self) -> &HashSet<LaneKey> {
        self.proxy_only_lanes.get_or_init(|| {
            built("proxy_only_lanes");
            crate::rfc_audit::bind_roles_of(&self.visible)
                .into_iter()
                .filter(|(_, roles)| roles.len() == 1 && roles.contains(&UaRole::Proxy))
                .map(|(lane, _)| lane)
                .collect()
        })
    }
}

/// Counts one projection build for the tests that bound them.
fn built(_projection: &'static str) {
    #[cfg(test)]
    crate::rfc_audit::builds::note(_projection);
}
