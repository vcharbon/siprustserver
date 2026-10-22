//! The receive-path parse and the depth-watermark selective intake shed: the
//! last-line guard below the admission layer (the self-gate is the first-line
//! brake).

use std::sync::Arc;

use sip_message::{CustomParser, Method, SipMessage};
use sip_net::types::{PreIngressAction, PreIngressHook};
use sip_net::{ParseOutcome, ParserPanicked};
use sip_txn::{class_of, InviteClass};

use crate::observability::ProxyMetrics;

/// The hook every signaling socket runs on each datagram before it is queued.
/// It parses the datagram, and the parse is queued with it, so the core does
/// not parse it again. At or above `watermark` queued datagrams it drops:
///
/// - a new non-emergency INVITE (`is_sheddable`), counted in
///   [`ProxyMetrics::intake_shed`];
/// - a datagram no response can answer — refused with nothing a 400 could
///   echo, or a parser panic — counted in
///   [`ProxyMetrics::intake_unanswerable`] and as the parse error or panic
///   the core would have counted.
///
/// In-dialog traffic, responses, other methods, emergency INVITEs and
/// answerable refusals (the core answers them with its capped 400) are
/// always queued.
pub fn intake_shed_hook(watermark: usize, metrics: Arc<ProxyMetrics>) -> PreIngressHook {
    let parser = CustomParser::new();
    Arc::new(move |arrival, _src, depth| {
        let parsed = arrival.parse(&parser);
        if depth < watermark {
            return PreIngressAction::Accept;
        }
        match parsed {
            Err(ParserPanicked) => {
                metrics.record_parse_panic();
                metrics.record_intake_unanswerable();
                PreIngressAction::Drop
            }
            Ok(Err(refusal)) if refusal.partial().is_none() => {
                metrics.record_parse_error();
                metrics.record_intake_unanswerable();
                PreIngressAction::Drop
            }
            parsed if is_sheddable(parsed) => {
                metrics.record_intake_shed();
                PreIngressAction::Drop
            }
            _ => PreIngressAction::Accept,
        }
    })
}

/// Whether `parsed` is the one class the shed drops: an INVITE of class
/// [`InviteClass::Normal`] — no To-tag, no emergency priority — the class the
/// B2BUA's ingress brake refuses.
fn is_sheddable(parsed: &ParseOutcome) -> bool {
    matches!(
        parsed,
        Ok(Ok(SipMessage::Request(req)))
            if *req.method() == Method::Invite && class_of(req) == InviteClass::Normal
    )
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
