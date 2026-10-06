//! Locally-authored response builders: the OPTIONS health reply, the
//! call-layer-stateless store-fault 500, the 481 and 405 a request naming
//! no call draws, the retry-later 500 and the merged-request 482. The 503
//! refusing a new INVITE is the admission ladder's
//! ([`crate::admission::Refusals`]).

use sip_message::generators::{generate_response, CapabilitySet, GenerateResponseOpts};
use sip_message::types::SipHeader;
use sip_message::{SipRequest, SipResponse};
use sip_txn::IdGen;

use crate::overload::OverloadSignal;
use crate::repl::{Readiness, ReadinessState};

/// Build a plain extension header.
fn hdr(name: &str, value: impl Into<String>) -> SipHeader {
    SipHeader { name: name.to_string().into(), value: value.into().into() }
}

/// `200 OK` to `req`, a request in a dialog, under the To-tag it carries.
pub(super) fn build_200(req: &SipRequest) -> SipResponse {
    generate_response(req, 200, "OK", &GenerateResponseOpts::default())
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
/// keepalive. Every reply mints a local To-tag: RFC 3261 §8.2.6.2 requires
/// a To-tag on any response > 100 to an out-of-dialog request (the 2xx path
/// always did; the 503 path needs it too). The status + `Reason` header text is the
/// contract `sip-proxy::health::probe::classify_503` keys on:
///   - `Ready`    → `200 OK` + `X-Overload: v=1; elu=…; gc=…; adm=…`.
///   - `NotReady` → `503` + `Reason: SIP;cause=503;text="not-ready"`.
///   - `Draining` → `503` + `Reason: SIP;cause=503;text="draining"` +
///     `Retry-After: 0`.
///
/// `capabilities` is the node's advertised `Allow`/`Supported`/`Accept` set
/// (§11.2) — node-scoped, since an out-of-dialog OPTIONS names no call, and
/// the one message this stack answers on its own behalf; `B2buaConfig`
/// declares it, defaulting to the stack set.
///
/// The `X-Overload` worker load signal rides the **200 path only**: it is the
/// live signal the proxy's ELU-band AIMD
/// (`sip_proxy::load_observer::parse_x_overload_header`) consumes to steer (and,
/// at `AboveCritical`, exclude) a *serving* worker. A 503 already removes the
/// node from new-dialog selection, so the band signal is not stamped there
/// (pinned by `options_200_stamps_x_overload_503_does_not`).
pub(crate) fn build_options_health_response(
    readiness: &Readiness,
    overload: &OverloadSignal,
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
    capabilities: &CapabilitySet,
) -> sip_message::SipResponse {
    let (status, reason, extra_headers): (u16, &str, Vec<SipHeader>) = match readiness.state() {
        ReadinessState::Ready => (
            200,
            "OK",
            // RFC 3261 §11.2: an OPTIONS 200 SHOULD advertise capabilities so the
            // querier learns method/extension/body support, not just liveness.
            // Plus the worker load signal the proxy's AIMD band reads.
            capabilities
                .lines()
                .into_iter()
                .map(|(name, value)| hdr(name.as_wire_str(), value))
                .chain([hdr("X-Overload", overload.x_overload_header_value())])
                .collect(),
        ),
        ReadinessState::NotReady => {
            (503, "Service Unavailable", vec![hdr("Reason", "SIP;cause=503;text=\"not-ready\"")])
        }
        ReadinessState::Draining => (
            503,
            "Service Unavailable",
            vec![hdr("Reason", "SIP;cause=503;text=\"draining\""), hdr("Retry-After", "0")],
        ),
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
