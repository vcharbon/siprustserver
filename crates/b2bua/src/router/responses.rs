//! Locally-authored response builders: the OPTIONS health reply and the two
//! call-layer-stateless rejects (overload 503, store-fault 500).

use sip_message::generators::{generate_response, GenerateResponseOpts};
use sip_txn::IdGen;

use crate::overload::OverloadSignal;
use crate::repl::{Readiness, ReadinessState};

/// Build the self-reported readiness reply to an out-of-dialog OPTIONS
/// keepalive (S7). Every reply mints a local To-tag: RFC 3261 §8.2.6.2 requires
/// a To-tag on any response > 100 to an out-of-dialog request (the 2xx path
/// always did; the 503 path needs it too, and `hydrate_response` rejects a
/// tagless response otherwise). The status + `Reason` header text is the
/// contract `sip-proxy::health::probe::classify_503` keys on:
///   - `Ready`    → `200 OK` + `X-Overload: v=1; elu=…; gc=…; adm=…`.
///   - `NotReady` → `503` + `Reason: SIP;cause=503;text="not-ready"`.
///   - `Draining` → `503` + `Reason: SIP;cause=503;text="draining"` +
///     `Retry-After: 0`.
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
) -> sip_message::SipResponse {
    use sip_message::types::SipHeader;

    let hdr = |name: &str, value: &str| SipHeader {
        name: name.to_string(),
        value: value.to_string(),
    };

    let (status, reason, extra_headers): (u16, &str, Vec<SipHeader>) = match readiness.state() {
        ReadinessState::Ready => (
            200,
            "OK",
            // RFC 3261 §11.2: an OPTIONS 200 SHOULD advertise capabilities so the
            // querier learns method/extension/body support, not just liveness.
            // Plus the worker load signal the proxy's AIMD band reads.
            vec![
                hdr("Allow", sip_message::generators::B2BUA_ALLOW),
                hdr("Accept", "application/sdp"),
                hdr("Supported", sip_message::generators::B2BUA_SUPPORTED),
                hdr("X-Overload", &overload.x_overload_header_value()),
            ],
        ),
        ReadinessState::NotReady => (
            503,
            "Service Unavailable",
            vec![hdr("Reason", "SIP;cause=503;text=\"not-ready\"")],
        ),
        ReadinessState::Draining => (
            503,
            "Service Unavailable",
            vec![
                hdr("Reason", "SIP;cause=503;text=\"draining\""),
                hdr("Retry-After", "0"),
            ],
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

/// Build the **stateless 503** the Tier-3 admission gate sends when it rejects a
/// new INVITE.
///
/// Stateless because no server transaction (and no call) is created — the router
/// sends this via [`TransactionLayer::send_raw`](sip_txn::TransactionLayer::send_raw)
/// and returns before `build_initial_call`. It echoes the INVITE's Via/From/To/
/// Call-ID/CSeq (via [`generate_response`]) and adds:
///   - `Reason: SIP;cause=503;text="overload"` — the overload cause token
///     (distinct from the readiness 503's `not-ready` / `draining`).
///   - `Retry-After: <retry_after_sec>` — the gate's hint (bucket time-to-token for
///     `bucket_empty`, the configured base for `panic_elu`).
///
/// Stamps a fresh To-tag: this codebase enforces a To-tag on every non-100 final
/// (RFC 3261 §8.2.6.2; `generate_response` adds a fallback, the RFC audit gate
/// flags a tagless final, and the sibling `reject_call` / readiness-503 paths
/// both tag). It stays stateless regardless: with no server txn the ACK still
/// can't match a dialog and is dropped at the orphan-ACK path, exactly the
/// cheap-rejection contract.
pub(super) fn build_stateless_overload_503(
    id_gen: &IdGen,
    req: &sip_message::SipRequest,
    retry_after_sec: u32,
) -> sip_message::SipResponse {
    use sip_message::types::SipHeader;
    let hdr = |name: &str, value: String| SipHeader { name: name.to_string(), value };
    generate_response(
        req,
        503,
        "Service Unavailable",
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            extra_headers: vec![
                hdr("Reason", "SIP;cause=503;text=\"overload\"".to_string()),
                hdr("Retry-After", retry_after_sec.to_string()),
            ],
            ..Default::default()
        },
    )
}

/// Build the fail-closed **500 Server Internal Error** for an initial INVITE
/// whose dialog-existence store lookup failed (ADR-0023). Same call-layer-
/// stateless shape as [`build_stateless_overload_503`]: sent through the INVITE
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
        &GenerateResponseOpts {
            to_tag: Some(id_gen.new_tag()),
            ..Default::default()
        },
    )
}
