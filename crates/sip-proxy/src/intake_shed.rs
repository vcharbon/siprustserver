//! The receive-path parse and the depth-watermark selective intake shed: the
//! last-line guard below the admission layer (the self-gate is the first-line
//! brake).

use std::sync::Arc;

use sip_message::{CustomParser, Method, SipMessage, SipRequest};
use sip_net::types::{PreIngressAction, PreIngressHook};
use sip_net::ParserPanicked;
use sip_txn::{class_of, InviteClass};

use crate::observability::ProxyMetrics;
use crate::worker_signs::WorkerSigns;

/// The face a hook serves, as it reads who sent a new INVITE.
#[derive(Clone)]
pub enum IntakeFace {
    /// The internal face, and the one face of a single-face proxy: a new
    /// INVITE carrying a worker sign ([`WorkerSigns::reads_as_a_workers`])
    /// belongs to a call a worker accepted and is never shed. A single-face
    /// proxy that strangers reach trusts these signs as its core does.
    Internal(WorkerSigns),
    /// The external face of a dual-face proxy, where its deployment keeps
    /// strangers: it trusts no worker sign.
    External,
}

/// The hook every signaling socket runs on each datagram before it is queued.
/// It parses the datagram, and the parse is queued with it, so the core does
/// not parse it again. At or above `watermark` queued datagrams it drops:
///
/// - a new non-emergency INVITE (`is_sheddable`) that carries no worker sign
///   on `face`, counted in [`ProxyMetrics::intake_shed`];
/// - a datagram no response can answer — refused with nothing a 400 could
///   echo, or a parser panic — counted in
///   [`ProxyMetrics::intake_unanswerable`] and as the parse error or panic
///   the core would have counted.
///
/// In-dialog traffic, responses, other methods, emergency INVITEs, a
/// worker's new INVITE and answerable refusals (the core answers them with
/// its capped 400) are always queued. A worker sign is what the core reads
/// as worker-outbound; a worker's INVITE that re-enters on a verified cookie
/// carries one too, is queued, and meets the core's self-gate. `parser` is
/// the process's one parser, the core's too.
pub fn intake_shed_hook(
    watermark: usize,
    metrics: Arc<ProxyMetrics>,
    parser: CustomParser,
    face: IntakeFace,
) -> PreIngressHook {
    Arc::new(move |arrival, src, load| {
        let parsed = arrival.parse(&parser);
        if load.depth < watermark {
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
            Ok(Ok(SipMessage::Request(req))) if is_sheddable(req) => match &face {
                IntakeFace::Internal(signs) if signs.reads_as_a_workers(req, src) => {
                    PreIngressAction::Accept
                }
                _ => {
                    metrics.record_intake_shed();
                    PreIngressAction::Drop
                }
            },
            _ => PreIngressAction::Accept,
        }
    })
}

/// Whether `req` is of the one class the shed drops: an INVITE of class
/// [`InviteClass::Normal`] — no To-tag, no emergency priority — the class the
/// B2BUA's ingress brake refuses.
fn is_sheddable(req: &SipRequest) -> bool {
    *req.method() == Method::Invite && class_of(req) == InviteClass::Normal
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
