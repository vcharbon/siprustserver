//! An event that resolved to no call: the answer RFC 3261 owes the peer, and
//! its accounting in classes that never overlap.
//!
//! - A wire request other than ACK is one a peer waits on, and draws
//!   [`refusal`] (`b2bua_unroutable_refused_total{method,code}`). A stray
//!   CANCEL's 481 is stateless, so each repeat of it counts again.
//! - A PRACK a released call still answers draws 200 instead
//!   ([`super::late_prack`], `b2bua_late_prack_answered_total`).
//! - An ACK or a response is owed nothing and is dropped
//!   (`b2bua_unroutable_dropped_total{kind}`).
//! - This node's own event — a client transaction released from its call
//!   (ADR-0034) reaching Timer B/F — crossed no wire and leaves no peer
//!   waiting (`b2bua_unroutable_internal_total{event,method}`).
//!
//! A request whose dialog lookup failed is no miss: it fails closed with the
//! keyed path's 500 (ADR-0023) and counts as a store fault, not here.
//!
//! Every event also lands in [`crate::lifecycle::UnroutableWaves`], so the log
//! names each class, why it resolved to nothing and one real example.

use std::net::SocketAddr;

use sip_message::{Method, SipMessage, SipRequest, SipResponse};

use super::process::refuse_on_store_fault;
use super::responses::{build_405, build_481};
use super::RouterCtx;
use crate::store::StoreFaultPoint;
use b2bua_sdk::event::CallEvent;

/// How the dialog lookups that found no call ended.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Lookup {
    /// Every read answered: the event names no call here.
    Answered,
    /// A store read failed: whether a call exists is unknown.
    Failed,
}

/// Why a request resolved to no call: `resolve` read no `callRef` in its
/// Request-URI and found no dialog index entry (nor, for a tagged request or a
/// CANCEL, a replica index entry).
const REQUEST_REASON: &str = "no-ruri-callref-no-index";
/// Why a response resolved to no call: no `cr` in its top Via and no dialog
/// index entry.
const RESPONSE_REASON: &str = "no-via-cr-no-index";
/// Why a transaction timeout resolved to no call: the release of its call
/// took the transaction off the call's books.
const RELEASED_TXN_REASON: &str = "txn-released-from-call";

/// Answer, count and log `event`, which resolved to no call; `lookup` says
/// whether the lookups that found none all answered.
pub(super) async fn on_unroutable(ctx: &RouterCtx, event: &CallEvent, lookup: Lookup) {
    match event {
        CallEvent::Sip { message, src, .. } => match message.as_ref() {
            SipMessage::Request(req) => {
                let probe = ctx.store_faults.check(StoreFaultPoint::LiveInDialog);
                if lookup == Lookup::Failed || probe.is_err() {
                    refuse_on_store_fault(ctx, req, *src, None).await;
                } else {
                    on_request(ctx, req, *src).await;
                }
            }
            SipMessage::Response(resp) => {
                let kind = status_class(resp.status());
                ctx.metrics.record_unroutable_dropped(&kind);
                ctx.unroutable_waves.record(
                    &format!("wire:{kind}"),
                    RESPONSE_REASON,
                    format!(
                        "call_id={} cseq_method={} src={src}",
                        resp.call_id().as_str(),
                        resp.cseq().method()
                    ),
                );
            }
        },
        CallEvent::Timeout { branch, method, destination, .. } => {
            let method = method.as_deref().map(Method::from_wire);
            let token = method.as_ref().map_or("", Method::as_str);
            ctx.metrics.record_unroutable_internal("timeout", token);
            let dest = destination.map_or_else(|| "-".to_string(), |d| d.to_string());
            let class = method.as_ref().map_or("", wave_label);
            ctx.unroutable_waves.record(
                &format!("internal:timeout:{class}"),
                RELEASED_TXN_REASON,
                format!("method={token} branch={branch} dest={dest}"),
            );
        }
        // `resolve` names a call for every other event kind; one reaching here
        // is still this node's own and counted as such.
        other => {
            ctx.metrics.record_unroutable_internal(other.kind(), "");
            ctx.unroutable_waves.record(
                &format!("internal:{}", other.kind()),
                "no-call-ref",
                String::new(),
            );
        }
    }
}

async fn on_request(ctx: &RouterCtx, req: &SipRequest, src: SocketAddr) {
    if let Some(answer) = super::late_prack::answer(ctx, req) {
        let _ = ctx.txn.send_response(answer, src).await;
        return;
    }
    let method = req.method().as_str();
    let class = wave_label(req.method());
    let sample = format!("method={method} call_id={} src={src}", req.call_id().as_str());
    match refusal(ctx, req) {
        Some(answer) => {
            let code = answer.status();
            let _ = ctx.txn.send_response(answer, src).await;
            ctx.metrics.record_unroutable_refused(method, code);
            ctx.unroutable_waves.record(&format!("wire:{class}:{code}"), REQUEST_REASON, sample);
        }
        None => {
            ctx.metrics.record_unroutable_dropped(method);
            ctx.unroutable_waves.record(&format!("wire:{class}"), REQUEST_REASON, sample);
        }
    }
}

/// A method's label in an unroutable wave's class: its canonical name, or
/// `other` for an extension method, so a peer cannot mint wave classes; the
/// token itself goes in the sample.
fn wave_label(method: &Method) -> &str {
    match method {
        Method::Other(_) => "other",
        known => known.as_str(),
    }
}

/// The answer a request naming no call here is owed; `None` for an ACK, which
/// draws no response (RFC 3261 §17.1.1.3).
///
/// - A To-tag names a dialog this node does not hold: 481 whatever the method
///   (§12.2.2), the answer that ends the peer's dialog (§12.2.1.2).
/// - A CANCEL matching no INVITE: 481 (§9.2), under the tag the transaction
///   layer binds from the INVITE's final, so none is chosen here.
/// - Any other untagged request, under a To-tag minted here (§8.2.6.2): 481
///   for a method in the node's `Allow` (a BYE, §15.1.2), else 405 with
///   `Allow` (§8.2.1, extension methods included, as the stack's
///   `unsupported-method-405-allow` rule holds). Methods compare
///   case-sensitively (§7.1).
pub(super) fn refusal(ctx: &RouterCtx, req: &SipRequest) -> Option<SipResponse> {
    let method = req.method();
    if method == Method::Ack {
        return None;
    }
    if req.to().tag().is_some() || method == Method::Cancel {
        return Some(build_481(req, None));
    }
    let minted = ctx.id_gen.new_tag();
    let caps = &ctx.config.node_capabilities;
    let allowed = caps.allow().map(|a| a.iter().any(|m| m == method.as_str()));
    Some(match allowed {
        Some(false) => build_405(req, Some(&minted), &caps.allow_text().unwrap_or_default()),
        Some(true) | None => build_481(req, Some(&minted)),
    })
}

/// A response's status class (`1xx` … `6xx`).
fn status_class(status: u16) -> String {
    format!("{}xx", status / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every extension method shares one wave class label.
    #[test]
    fn an_extension_method_mints_no_wave_class() {
        assert_eq!(wave_label(&Method::from_wire("X1")), "other");
        assert_eq!(wave_label(&Method::from_wire("X2")), "other");
        assert_eq!(wave_label(&Method::Bye), "BYE");
    }
}
