//! Locally-authored response builders: the OPTIONS health reply, the
//! call-layer-stateless store-fault 500 and the orphan 481. The overload reject
//! lives with the policy that owns it — [`crate::overload::build_reject_new_call_503`].

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

/// `481 Call/Transaction Does Not Exist` to `req`: an in-dialog request naming
/// no dialog this node holds (RFC 3261 §12.2.2), or a CANCEL matching no INVITE
/// transaction here (§9.2). `to_tag` is the tag this node's final to the
/// INVITE carried, when a call resolves and `req`'s To has none (§9.2: the
/// CANCEL's response shares that tag); a tagged To is echoed as it came.
pub(super) fn build_481(req: &SipRequest, to_tag: Option<&str>) -> SipResponse {
    let opts = GenerateResponseOpts { to_tag: to_tag.map(str::to_owned), ..Default::default() };
    generate_response(req, 481, "Call/Transaction Does Not Exist", &opts)
}

/// Build the self-reported readiness reply to an out-of-dialog OPTIONS
/// keepalive (S7). Every reply mints a local To-tag: RFC 3261 §8.2.6.2 requires
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
/// (tracked divergence — pinned by `options_200_stamps_x_overload_503_does_not`;
/// revisit with the AIMD rate-cap consumer, see `MIGRATION_STATUS.md`).
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
/// whose dialog-existence store lookup failed (ADR-0023). Same call-layer-
/// stateless shape as [`crate::overload::build_reject_new_call_503`]: sent
/// through the INVITE
/// server txn (`send_response` supersedes the cached 100, retransmits the final
/// and absorbs the ACK) with **no** call/dialog/CDR/limiter state born. Fresh
/// To-tag — this codebase enforces a tag on every non-100 final (RFC 3261
/// §8.2.6.2). No Reason header: the bare canonical reject (ADR-0022 X3 shape);
/// the fault is observable via `b2bua_store_fault_rejected_total`.
pub(super) fn build_store_fault_500(
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
) -> sip_message::SipResponse {
    generate_response(
        req,
        500,
        "Server Internal Error",
        &GenerateResponseOpts { to_tag: Some(id_gen.new_tag()), ..Default::default() },
    )
}
