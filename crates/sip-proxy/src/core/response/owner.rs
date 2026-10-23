//! The owner rule on the response path (ADR-0055 §8): a response bound for a
//! worker goes to the owner that holds its transaction; if that owner is not
//! alive, to the call's other owner; a call with one owner goes to it. The
//! rule is the same for every method.
//!
//! The owners are read off the cookie of the Via this proxy pushed on the
//! request (`core/request/via_cookie`): the worker that sent it and the call's
//! other owner. Without one there, off the proxy's own Record-Route entry
//! naming the worker the response is for. A response whose Via carries no
//! cookie and names no worker — a stranger's request — follows its Via.
//!
//! The owner holding the transaction is the worker the response's next Via
//! names by its sent-by, when that worker is one of the cookie's; else, the
//! address having stopped naming it, the cookie's primary. It holds its
//! transaction while the proxy does not read it `Dead`: a departed address
//! reads `Dead` for its window (`registry::tombstone`). Past that, the
//! strategy places the response as it places a request on the cookie
//! (`RoutingStrategy::decode_stickiness`): the cookie's two workers are the
//! call's two owners, and the one it picks while the holder is not alive is
//! the other.

use sip_message::header::Via;
use sip_message::SipMessage;

use crate::addr::ProxyAddr;
use crate::headers::{cookie_params, via_cookie_params};
use crate::registry::{WorkerEntry, WorkerHealth};
use crate::strategy::{DecodeResult, RouteParams};

use super::super::ProxyCore;

/// Where a response bound for a worker goes.
pub(super) enum OwnerRoute {
    /// Along its next Via: the owner holding its transaction is there, or no
    /// cookie names the call's owners.
    Via,
    /// To this address: the holder's own, moved since, or the other owner's.
    To(ProxyAddr),
    /// No owner can take it.
    Nowhere,
}

impl ProxyCore {
    /// The owner rule for `msg`, a response whose top Via is `ours` and whose
    /// next Via's sent-by is `sent_by`.
    pub(super) async fn owner_route(
        &self,
        msg: &SipMessage,
        ours: &Via,
        sent_by: &ProxyAddr,
    ) -> OwnerRoute {
        let named = self.registry.lookup_by_address(sent_by);
        let dead = named.as_ref().filter(|n| n.health == WorkerHealth::Dead);
        let cookie = self
            .strategy
            .verified_cookie(&via_cookie_params(ours), msg)
            .or_else(|| self.record_route_cookie(msg, &dead?.id));
        let Some(cookie) = cookie else {
            return match dead {
                Some(dead) => self.moved(dead).map_or(OwnerRoute::Nowhere, OwnerRoute::To),
                None => OwnerRoute::Via,
            };
        };
        let primary = self.strategy.stickiness_primary(&cookie).unwrap_or_default();
        let backup = self.strategy.stickiness_backup(&cookie).unwrap_or_default();
        let holder = named
            .as_ref()
            .filter(|n| n.id == primary || n.id == backup)
            .map_or(primary, |n| n.id.as_str());
        if named.as_ref().is_some_and(|n| n.id == holder && n.health != WorkerHealth::Dead) {
            return OwnerRoute::Via;
        }
        let at_now = self.registry.resolve(holder).filter(|w| w.health != WorkerHealth::Dead);
        if let Some(holder) = at_now {
            return OwnerRoute::To(holder.address);
        }
        match self.strategy.decode_stickiness(&cookie, msg).await {
            DecodeResult::Forward { target, .. } | DecodeResult::ForwardBackup { target, .. } => {
                OwnerRoute::To(target)
            }
            DecodeResult::Reject { .. }
            | DecodeResult::Unknown { .. }
            | DecodeResult::Unroutable => OwnerRoute::Nowhere,
        }
    }

    /// The address the worker `dead` names holds now, when it is a member at
    /// another address the proxy does not read `Dead`.
    fn moved(&self, dead: &WorkerEntry) -> Option<ProxyAddr> {
        self.registry
            .resolve(&dead.id)
            .filter(|w| w.address != dead.address && w.health != WorkerHealth::Dead)
            .map(|w| w.address)
    }

    /// The cookie of the bottommost of the proxy's own Record-Route entries
    /// (either face) whose cookie names `worker` as primary (why the
    /// bottommost: ADR-0009 X1). The strategy verifies it when it places the
    /// response on it.
    fn record_route_cookie(&self, msg: &SipMessage, worker: &str) -> Option<RouteParams> {
        let SipMessage::Response(resp) = msg else { return None };
        resp.list::<sip_message::header::RecordRouteEntry>()
            .unwrap_or_default()
            .iter()
            .rev()
            .filter(|entry| {
                let (host, port) = entry.uri().host_port();
                self.is_self_addr(host, port)
            })
            .map(|entry| cookie_params(entry.uri()))
            .find(|params| self.strategy.stickiness_primary(params) == Some(worker))
    }
}
