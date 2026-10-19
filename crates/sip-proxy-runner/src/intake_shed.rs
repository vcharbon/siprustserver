//! The depth-watermark selective intake shed: the last-line guard below the
//! admission layer (the self-gate is the first-line brake).

use std::sync::Arc;

use sip_net::types::{PreIngressAction, PreIngressHook};

/// At or above `watermark` queued datagrams the socket pump drops only what
/// [`sip_message::sniff::is_sheddable_new_invite`] classifies as a NEW
/// non-emergency INVITE; in-dialog traffic, responses, ACK/BYE/CANCEL,
/// emergency INVITEs, and garbage are always admitted. Drops land on the
/// endpoint's `pre_ingress_dropped` counter → `sip_proxy_intake_shed_total`.
pub fn intake_shed_hook(watermark: usize) -> PreIngressHook {
    Arc::new(move |raw, _src, depth| {
        if depth >= watermark && sip_message::sniff::is_sheddable_new_invite(raw) {
            PreIngressAction::Drop
        } else {
            PreIngressAction::Accept
        }
    })
}

/// The shed watermark in queued datagrams: `pct` percent of `queue_max`,
/// floored at 1 so `0%` sheds whenever anything is queued rather than on an
/// idle socket; `100%` only sheds at a full queue.
pub fn intake_shed_watermark(queue_max: usize, pct: u64) -> usize {
    let pct = usize::try_from(pct).unwrap_or(usize::MAX);
    (queue_max.saturating_mul(pct) / 100).max(1)
}

#[cfg(test)]
mod tests;
