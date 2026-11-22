//! The proxy's own answer to a supervision OPTIONS (ADR-0069, RFC 3261 §11):
//! an out-of-dialog OPTIONS whose Request-URI names this proxy with no user
//! part. It says whether the POP behind this proxy serves, so it is answered
//! here, never relayed to a worker as a new dialog.
//!
//! - While a worker reads `Alive` or `Slow`, once the first view is in: `200`
//!   with a To-tag, stating the `Allow`, `Supported` and `Accept` the workers
//!   last advertised, whatever their capacity. It meets no self-gate and spends
//!   no worker's token.
//! - Otherwise it is dropped unanswered and counted, as a request with no
//!   target is (ADR-0050 §5): the node's withdrawal and the supervisor's own
//!   timeout take the POP out, never a `503` a healthy peer proxy would not
//!   send.
//!
//! The Request-URI names this proxy by either face's advertise or bind
//! address, its port, when stated, one of theirs, and carries no parameter
//! but `transport` and no header. Every other OPTIONS is relayed.

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_message::header::Uri;
use sip_message::ip_literal::same_host;
use sip_message::received::observed_source;
use sip_message::{Method, SipRequest};

use crate::observability::metrics::{LocalOptions, RoutingDecisionKind};

use super::super::ProxyCore;
use super::{Arrival, RouteOutcome};

/// The one Request-URI parameter a supervision OPTIONS may carry.
const TRANSPORT: &str = "transport";

impl ProxyCore {
    /// Answer `req` when it is a supervision OPTIONS and this proxy answers
    /// them, or drop it when no worker serves; `None` for any other request.
    pub(super) async fn answer_supervision(
        &self,
        req: &SipRequest,
        arrival: Arrival,
    ) -> Option<RouteOutcome> {
        let capabilities = self.supervision.as_ref()?;
        if req.method() != Method::Options
            || req.to().tag().is_some()
            || !self.names_this_proxy(req.request_uri())
        {
            return None;
        }
        let serving = self.registry.snapshot().iter().any(|w| w.serves_dialogs());
        if !serving || !self.first_view.is_in() {
            self.metrics.record_local_options(LocalOptions::Dropped);
            return Some(RouteOutcome { decision: RoutingDecisionKind::Reject, target: None });
        }
        let opts = GenerateResponseOpts {
            to_tag: Some(self.refusals.to_tag(req)),
            extra_headers: capabilities.lines(),
            incoming_source: Some(observed_source(arrival.src)),
            ..Default::default()
        };
        if let Ok(resp) = generate_response(req, 200, "OK", &opts) {
            self.send_own_final_to(req.call_id().as_str(), arrival, &resp).await;
        }
        self.metrics.record_local_options(LocalOptions::Ok);
        Some(RouteOutcome { decision: RoutingDecisionKind::LocalAnswer, target: None })
    }

    /// Whether `uri` names this proxy with no user part: a host either face
    /// advertises or is bound to, at one of their ports or none stated, with
    /// no parameter but `transport` and no header.
    fn names_this_proxy(&self, uri: &Uri) -> bool {
        if !uri.is_sip_or_sips() || uri.user().is_some() || uri.escaped_headers().next().is_some() {
            return false;
        }
        if uri.params().iter().any(|(name, _)| !name.as_str().eq_ignore_ascii_case(TRANSPORT)) {
            return false;
        }
        let (host, port) = (uri.host(), uri.port());
        self.own_points().iter().any(|(own, own_port)| {
            same_host(host, own) && port.is_none_or(|stated| stated == *own_port)
        })
    }

    /// Every `(host, port)` this proxy answers at: each face's advertise and
    /// its bound address, an unspecified bind left out.
    fn own_points(&self) -> Vec<(String, u16)> {
        let mut points = vec![(self.advertised.host.clone(), self.advertised.port)];
        let mut bound = vec![self.endpoint.local_addr()];
        if let Some(ext) = &self.external {
            points.push((ext.advertised.host.clone(), ext.advertised.port));
            bound.push(ext.endpoint.local_addr());
        }
        points.extend(
            bound
                .into_iter()
                .filter(|at| !at.ip().is_unspecified())
                .map(|at| (at.ip().to_string(), at.port())),
        );
        points
    }
}
