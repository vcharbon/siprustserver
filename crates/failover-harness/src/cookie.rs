//! The load-balancing proxy's stickiness cookie, read back off a forwarded
//! request. The b-leg a worker sends carries the proxy's own Record-Route, and
//! its URI parameters name which worker is primary and which is backup — the
//! authoritative binding a failover test needs before it can address a node.

use sip_message::types::SipRequest;

/// One named field of the stickiness cookie on the topmost Record-Route entry
/// stating it, a hop past the proxy recording above it. `None` when the request
/// records no route, when no reader accepts the entries, or when no cookie
/// states the field.
pub fn cookie_field(req: &SipRequest, name: &str) -> Option<String> {
    let recorded = req.record_route_set().ok()?;
    let value = recorded.iter().find_map(|entry| entry.uri().param(name).cloned())?;
    Some(value.as_str().unwrap_or_default().to_string())
}

/// The `(w_pri, w_bak)` worker ordinals the proxy stamped on the route it
/// recorded. An absent field reads as the empty string, which names no worker.
pub fn worker_ordinals(req: &SipRequest) -> (String, String) {
    (cookie_field(req, "w_pri").unwrap_or_default(), cookie_field(req, "w_bak").unwrap_or_default())
}
