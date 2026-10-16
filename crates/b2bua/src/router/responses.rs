//! Locally-authored response builders: the OPTIONS health reply, the
//! call-layer-stateless store-fault 500, the 481 and 405 a request naming
//! no call draws, the 200 and 487 an ending dialog answers, the out-of-order
//! 500, the retry-later 500 and the merged-request 482. The 503 refusing a new INVITE is the admission
//! ladder's ([`crate::admission::Refusals`]).

use sip_message::generators::{generate_response, CapabilitySet, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::worker_report::{
    incarnation_value, Readiness as WorkerReadiness, X_INCARNATION, X_READINESS,
};
use sip_message::{SipRequest, SipResponse};
use sip_txn::IdGen;

use crate::overload::OverloadSignal;
use crate::repl::{OptionsReport, Readiness};

/// Build a plain extension header.
fn hdr(name: &str, value: impl Into<String>) -> SipHeader {
    SipHeader { name: name.to_string().into(), value: value.into().into() }
}

/// `200 OK` to `req`, a request in a dialog, under the To-tag it carries.
pub(super) fn build_200(req: &SipRequest) -> SipResponse {
    generate_response(req, 200, "OK", &GenerateResponseOpts::default())
}

/// `200 OK` to `req`, a request in a dialog, stating `advertisement` (RFC
/// 3261 §11.2: an OPTIONS answer names what this stack supports).
pub(super) fn build_200_advertising(
    req: &SipRequest,
    advertisement: &CapabilitySet,
) -> SipResponse {
    let opts = GenerateResponseOpts {
        extra_headers: minted_final_lines(advertisement),
        ..Default::default()
    };
    generate_response(req, 200, "OK", &opts)
}

/// `487 Request Terminated` to `req`, a request in a dialog this stack is
/// ending by a BYE or a CANCEL (RFC 3261 §21.4.25).
pub(super) fn build_487(req: &SipRequest) -> SipResponse {
    generate_response(req, 487, "Request Terminated", &GenerateResponseOpts::default())
}

/// `481 Call/Transaction Does Not Exist` to `req`: an in-dialog request naming
/// no dialog this node holds (RFC 3261 §12.2.2), or a CANCEL matching no INVITE
/// transaction here (§9.2). `to_tag` is the tag this node's final to the
/// INVITE carried, when a call resolves and `req`'s To has none (§9.2: the
/// CANCEL's response shares that tag); a tagged To is echoed as it came.
pub(super) fn build_481(req: &SipRequest, to_tag: Option<&str>) -> SipResponse {
    let opts = GenerateResponseOpts { to_tag: to_tag.map(str::to_owned), ..Default::default() };
    generate_response(req, 481, "Call/Transaction Does Not Exist", &opts)
}

/// `500 Server Internal Error` to `req`, an in-dialog request whose CSeq is
/// below the last one its dialog took: out of order (RFC 3261 §12.2.2).
pub(super) fn build_out_of_order_500(req: &SipRequest) -> SipResponse {
    generate_response(req, 500, "Server Internal Error", &GenerateResponseOpts::default())
}

/// `500 Server Internal Error` to `req` with `Retry-After: retry_after_sec`,
/// floored at 1 s: a request this node took but could not process now, which
/// the UAC may retry (RFC 3261 §14.2 for a re-INVITE, §21.5.1). `to_tag` is
/// the tag minted for a request whose To has none (§8.2.6.2).
pub(super) fn build_retry_later_500(
    req: &SipRequest,
    to_tag: Option<String>,
    retry_after_sec: u32,
) -> SipResponse {
    let opts = GenerateResponseOpts {
        to_tag,
        extra_headers: vec![hdr(
            "Retry-After",
            load_shed::retry_after::floored(retry_after_sec).to_string(),
        )],
        ..Default::default()
    };
    generate_response(req, 500, "Server Internal Error", &opts)
}

/// `482 Loop Detected` to `req`, an initial INVITE merged with one already
/// here: same Call-ID, From-tag and CSeq on another branch (RFC 3261
/// §8.2.2.2), under a fresh To-tag (§8.2.6.2), carrying the deployment's
/// `advertisement` for a minted final.
pub(super) fn build_merged_482(
    id_gen: &IdGen,
    req: &SipRequest,
    advertisement: &CapabilitySet,
) -> SipResponse {
    let opts = GenerateResponseOpts {
        to_tag: Some(id_gen.new_tag()),
        extra_headers: minted_final_lines(advertisement),
        ..Default::default()
    };
    generate_response(req, 482, "Loop Detected", &opts)
}

/// The deployment's minted-final advertisement as header lines.
pub(crate) fn minted_final_lines(advertisement: &CapabilitySet) -> Vec<SipHeader> {
    advertisement.lines().into_iter().map(|(name, value)| hdr(name.as_wire_str(), value)).collect()
}

/// `405 Method Not Allowed` to `req`, a request of a method the node does not
/// serve (RFC 3261 §8.2.1), stating `allow` — the node's `Allow` — and under
/// `to_tag` when `req`'s To has none (§8.2.6.2).
pub(super) fn build_405(req: &SipRequest, to_tag: Option<&str>, allow: &str) -> SipResponse {
    let opts = GenerateResponseOpts {
        to_tag: to_tag.map(str::to_owned),
        extra_headers: vec![hdr("Allow", allow)],
        ..Default::default()
    };
    generate_response(req, 405, "Method Not Allowed", &opts)
}

/// Build the self-reported readiness reply to an out-of-dialog OPTIONS
/// keepalive, on the two axes of [`Readiness::options_report`] (ADR-0050 §2).
/// Every reply mints a local To-tag (RFC 3261 §8.2.6.2) and states the
/// process's boot generation (`X-Incarnation`), so a proxy tells a restart
/// at the same address from the process it last read:
///   - serving dialogs (Ready, Draining) → `200 OK` with the capabilities
///     (§11.2), `X-Overload: v=1; elu=…; gc=…; adm=…` and `X-Readiness:
///     new=accept`, or `new=refuse; cause=…` while it refuses new calls;
///   - NotReady → `503` and nothing more: it serves nothing yet.
///
/// `capabilities` is the node's advertised `Allow`/`Supported`/`Accept` set —
/// node-scoped, since an out-of-dialog OPTIONS names no call; `B2buaConfig`
/// declares it, defaulting to the stack set. The `X-Overload` payload rides
/// every `200`, a refusing node's included, so the proxy's stale-payload sweep
/// never floors a node that answers (ADR-0050 §6).
pub(crate) fn build_options_health_response(
    readiness: &Readiness,
    overload: &OverloadSignal,
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
    capabilities: &CapabilitySet,
) -> sip_message::SipResponse {
    let incarnation = hdr(X_INCARNATION, incarnation_value(readiness.incarnation()));
    let (status, reason, extra_headers): (u16, &str, Vec<SipHeader>) =
        match readiness.options_report() {
            OptionsReport::Serving(refusal) => {
                let report = match refusal {
                    None => WorkerReadiness::accept(),
                    Some(refusal) => WorkerReadiness::refuse(refusal.cause()),
                };
                (
                    200,
                    "OK",
                    capabilities
                        .lines()
                        .into_iter()
                        .map(|(name, value)| hdr(name.as_wire_str(), value))
                        .chain([
                            hdr("X-Overload", overload.x_overload_header_value()),
                            hdr(X_READINESS, report.header_value()),
                            incarnation,
                        ])
                        .collect(),
                )
            }
            OptionsReport::NotReady => (503, "Service Unavailable", vec![incarnation]),
        };

    generate_response(
        req,
        status,
        reason,
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            extra_headers,
            ..Default::default()
        },
    )
}

/// Build the fail-closed **500 Server Internal Error** for an initial INVITE
/// whose dialog-existence store lookup failed (ADR-0023). Call-layer
/// stateless like an admission refusal: sent through the INVITE server txn (`send_response` supersedes the cached 100, retransmits the final
/// and absorbs the ACK) with **no** call/dialog/CDR/limiter state born. Fresh
/// To-tag — this codebase enforces a tag on every non-100 final (RFC 3261
/// §8.2.6.2). No Reason header: the bare canonical reject (ADR-0022 X3 shape);
/// the fault is observable via `b2bua_store_fault_rejected_total`. The
/// deployment's `advertisement` for a minted final rides.
pub(super) fn build_store_fault_500(
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
    advertisement: &CapabilitySet,
) -> sip_message::SipResponse {
    generate_response(
        req,
        500,
        "Server Internal Error",
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            extra_headers: minted_final_lines(advertisement),
            ..Default::default()
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use sip_message::header::{AcceptRange, HeaderName};
    use sip_message::{CustomParser, SipMessage, SipParser};

    fn invite() -> SipRequest {
        let raw = "INVITE sip:bob@127.0.0.1:5060 SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.1:5555;branch=z9hG4bK-r\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@caller.test>;tag=a\r\n\
To: <sip:bob@b2bua.test>\r\n\
Call-ID: r@10.0.0.1\r\n\
CSeq: 1 INVITE\r\n\
Content-Length: 0\r\n\r\n";
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture parses") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    fn policy() -> CapabilitySet {
        CapabilitySet::stating(None, None, Some(vec![AcceptRange::new("application/sdp")]))
    }

    /// The merged-request 482 and the store-fault 500 are finals minted in the
    /// worker's own name: each carries the deployment's advertisement.
    #[test]
    fn the_stateless_minted_finals_carry_the_advertisement() {
        let id_gen = IdGen::seeded(1);
        for resp in [
            build_merged_482(&id_gen, &invite(), &policy()),
            build_store_fault_500(&id_gen, &invite(), &policy()),
        ] {
            let accept: Vec<&str> = resp.raw(HeaderName::Accept).collect();
            assert_eq!(accept, ["application/sdp"], "{}", resp.status());
        }
    }
}
