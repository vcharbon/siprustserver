//! Leg "stack identity" — port of `src/b2bua/stack-identity.ts`. Stamps the
//! B2BUA's own Via + Contact onto outbound messages, carrying the `callRef`
//! (`cr`), the leg id (`lg`) and the call incarnation's mark (`ci`) in opaque
//! params so any inbound response/request self-routes back to the owning call
//! without an external lookup, and a straggler of an earlier call on the same
//! `callRef` is told from the live one. The Via also states the call's other
//! owner (`bak`), which the front proxy signs into the leg's stickiness cookie
//! (ADR-0055 §8), and, signed with the cluster key, the leg and the request's
//! Request-URI (`vx`, ADR-0055 §12): a node that holds no record of the
//! request reads them off its response.
//!
//! `callRef` contains `|` (and Call-IDs may contain `@`/`:`), which are unsafe
//! in a SIP param value, so the values are percent-encoded here and decoded by
//! [`decode_param`] on the read path.

use crate::config::B2buaConfig;
use sip_ids::SigningKey;
use sip_message::header::{ParamValue, Uri, Via};
use sip_message::via_extension::{self, ViaExtension};
use sip_message::SipStr;
// The cr/lg/callRef param codec lives in sip-message so the encoder and its
// inverse can't drift across crates; re-export so existing `stack_identity::`
// callers (the router's read path) are unchanged.
pub use sip_message::param_codec::{decode_param, encode_param};

/// Inputs of the Via builder.
pub struct StackIdentityOpts<'a> {
    pub local_ip: &'a str,
    pub local_port: u16,
    pub call_ref: &'a str,
    pub leg: &'a str,
    /// The call's [`call::Call::incarnation_mark`].
    pub incarnation_mark: &'a str,
    pub is_emergency: bool,
    /// The worker that owns the call beside the sender, `None` for a call
    /// with one owner.
    pub other_owner: Option<&'a str>,
    /// What the request the Via rides states about itself — its Request-URI
    /// and whether it carries a session offer — with the cluster key that
    /// signs it into the via-extension; `None` states none.
    pub request: Option<(RequestFacts<'a>, &'a SigningKey)>,
}

/// What an outbound request states about itself in the via-extension.
#[derive(Clone, Copy, Debug)]
pub struct RequestFacts<'a> {
    /// The request's method, as its CSeq states it.
    pub method: &'a str,
    /// The request's Request-URI.
    pub uri: &'a str,
    /// Whether the request carries a session offer (RFC 3264 §4).
    pub offer: bool,
}

/// A parameter carrying an encoded value.
fn encoded(value: &str) -> ParamValue {
    ParamValue::Token(SipStr::owned(&encode_param(value)))
}

/// Build the B2BUA Via for an outbound message (with `cr`/`lg`/`ci`/`rport`
/// params, `bak` naming the call's other owner when it has one, and the
/// signed `vx` when the request states its Request-URI and it fits).
pub fn build_call_via(opts: &StackIdentityOpts, branch: String) -> Via {
    let cr = encode_param(opts.call_ref);
    let mut via = Via::udp(SipStr::owned(opts.local_ip), opts.local_port)
        .with_branch(SipStr::owned(&branch))
        .with_param("cr", ParamValue::Token(SipStr::owned(&cr)))
        .with_param("lg", encoded(opts.leg))
        .with_param("ci", encoded(opts.incarnation_mark))
        .requesting_rport();
    if opts.is_emergency {
        via = via.with_param("em", ParamValue::Token(SipStr::from_static("1")));
    }
    if let Some(owner) = opts.other_owner.filter(|o| !o.is_empty()) {
        via = via.with_param("bak", encoded(owner));
    }
    if let Some((facts, key)) = opts.request {
        let ext = ViaExtension {
            leg: opts.leg.to_string(),
            request_uri: facts.uri.to_string(),
            offer: facts.offer,
        };
        let stated = Stated {
            call_ref: opts.call_ref,
            leg: opts.leg,
            incarnation_mark: opts.incarnation_mark,
            branch: &branch,
            method: facts.method,
        };
        let signature = key.sign(&signed_parts(&stated, &ext));
        if let Some(value) = ext.param(&signature) {
            via = via.with_param(via_extension::PARAM, value);
        }
    }
    via
}

/// This node's Via, at `local_ip:local_port` on `branch`, for a request of the
/// call and leg another of the stack's Vias names — the one a final for a
/// call no node holds rides (ADR-0055 §4): the call, leg, incarnation and
/// emergency marks `via` carries, decoded and stamped again, no other owner
/// and no via-extension. `None` when `via` names no call.
pub fn call_via_of(via: &Via, local_ip: &str, local_port: u16, branch: String) -> Option<Via> {
    let decoded = |name: &str| via.param(name).and_then(ParamValue::as_str).map(decode_param);
    let (call_ref, leg, incarnation_mark) = (decoded("cr")?, decoded("lg")?, decoded("ci")?);
    let opts = StackIdentityOpts {
        local_ip,
        local_port,
        call_ref: &call_ref,
        leg: &leg,
        incarnation_mark: &incarnation_mark,
        is_emergency: via.param("em").is_some(),
        other_owner: None,
        request: None,
    };
    Some(build_call_via(&opts, branch))
}

/// The via-extension `via` carries on a response to a `method` request, when
/// its signature verifies under `key` for the call, leg, incarnation and
/// branch the Via names; `None` without a key, an extension, or a signature
/// that verifies (ADR-0055 §12: a failed signature is ignored).
pub fn read_via_extension(
    via: &Via,
    method: &str,
    key: Option<&SigningKey>,
) -> Option<ViaExtension> {
    let key = key?;
    let decoded = |name: &str| via.param(name).and_then(ParamValue::as_str).map(decode_param);
    let (call_ref, leg, incarnation_mark) = (decoded("cr")?, decoded("lg")?, decoded("ci")?);
    let stated = Stated {
        call_ref: &call_ref,
        leg: &leg,
        incarnation_mark: &incarnation_mark,
        branch: via.branch()?,
        method,
    };
    let (ext, signature) = via_extension::read(via)?;
    key.verify(&signed_parts(&stated, &ext), &signature).then_some(ext)
}

/// What the Via states beside the extension, decoded, that its signature
/// binds the extension to.
struct Stated<'a> {
    call_ref: &'a str,
    leg: &'a str,
    incarnation_mark: &'a str,
    branch: &'a str,
    method: &'a str,
}

/// What the via-extension's signature covers: the call, leg and incarnation
/// the Via names, its branch, the request's method, and the extension's leg,
/// Request-URI and offer flag.
fn signed_parts<'a>(stated: &'a Stated, ext: &'a ViaExtension) -> [&'a [u8]; 8] {
    [
        stated.call_ref.as_bytes(),
        stated.leg.as_bytes(),
        stated.incarnation_mark.as_bytes(),
        stated.branch.as_bytes(),
        stated.method.as_bytes(),
        ext.leg.as_bytes(),
        ext.request_uri.as_bytes(),
        ext.offer_flag().as_bytes(),
    ]
}

/// Inputs of the Contact builder: the address the Contact names and the
/// call's markers.
pub struct ContactFacts<'a> {
    pub host: &'a str,
    pub port: u16,
    pub call_ref: &'a str,
    pub leg: &'a str,
    /// The call's [`call::Call::incarnation_mark`].
    pub incarnation_mark: &'a str,
    pub is_emergency: bool,
}

/// Build the B2BUA Contact URI of a leg (with `callRef`/`leg`/`ci` params).
pub fn build_call_contact(facts: &ContactFacts) -> Uri {
    let mut uri = Uri::sip_user(SipStr::from_static("b2bua"), SipStr::owned(facts.host))
        .with_port(facts.port)
        .with_param("callRef", encoded(facts.call_ref))
        .with_param("leg", encoded(facts.leg))
        .with_param("ci", encoded(facts.incarnation_mark));
    if facts.is_emergency {
        uri = uri.with_param("emerg", ParamValue::Token(SipStr::from_static("1")));
    }
    uri
}

// ---------------------------------------------------------------------------
// Public read-side seam — consumer-facing API for the addresses the B2BUA
// stamps on outbound Contact / Via (port of `StackIdentity` /
// `StackIdentityApi`).
// ---------------------------------------------------------------------------

/// Read-only view of the addresses this B2BUA advertises to its peers.
/// Consumers running their own templating layer (e.g. resolving address
/// placeholders in their call-control payloads) read these once at startup,
/// then hand fully-resolved literals to the decision engine.
///
/// A pure config projection: a cheap value with accessors, mirroring
/// `sip_proxy`'s `ProxyCore::advertised()`. [`StackIdentity::from_config`]
/// derives the advertised host/port from [`B2buaConfig::sip_local_ip`] /
/// [`B2buaConfig::sip_local_port`], the node's own address; behind the proxy
/// the Contact names [`B2buaConfig::contact_address`] instead.
///
/// This is a **consumer seam** with no in-tree caller (the outbound builders
/// above read the same fields straight off [`B2buaConfig`]). It exists so a
/// consumer running its own templating layer has a stable contract to read once
/// at startup; do not delete it as dead code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackIdentity {
    advertised_host: String,
    advertised_port: u16,
}

impl StackIdentity {
    /// Derive the advertised identity from a worker's [`B2buaConfig`] — the
    /// analogue of `StackIdentity.Default` wired over `AppConfig`.
    pub fn from_config(config: &B2buaConfig) -> Self {
        Self {
            advertised_host: config.sip_local_ip.clone(),
            advertised_port: config.sip_local_port,
        }
    }

    /// Host the B2BUA stamps on outbound Via (`advertisedHost`):
    /// [`B2buaConfig::sip_local_ip`].
    pub fn advertised_host(&self) -> &str {
        &self.advertised_host
    }

    /// Port the B2BUA stamps on outbound Via (`advertisedPort`):
    /// [`B2buaConfig::sip_local_port`].
    pub fn advertised_port(&self) -> u16 {
        self.advertised_port
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callref_param_round_trips() {
        let call_ref = "w0|alice@example.com:5060|ab12cd34";
        let enc = encode_param(call_ref);
        assert!(!enc.contains('|') && !enc.contains('@'));
        assert_eq!(decode_param(&enc), call_ref);
    }

    #[test]
    fn via_carries_cr_lg_ci_rport() {
        let via = build_call_via(&opts(false), "z9hG4bKabc".to_string());
        assert_eq!(param_names(via.params()), vec!["branch", "cr", "lg", "ci", "rport"]);
        assert_eq!(via.param("ci").and_then(ParamValue::as_str), Some("k3x9"));
        assert_eq!(via.branch(), Some("z9hG4bKabc"));
    }

    fn facts(uri: &str) -> RequestFacts<'_> {
        RequestFacts { method: "INVITE", uri, offer: true }
    }

    fn key(byte: u8) -> SigningKey {
        SigningKey::new(&[byte; 32]).expect("a 32-byte key")
    }

    #[test]
    fn a_request_stating_its_uri_carries_the_signed_extension() {
        let k = key(1);
        let o = StackIdentityOpts {
            request: Some((facts("sip:bob@10.0.0.2:5070"), &k)),
            ..opts(false)
        };
        let via = build_call_via(&o, "z9hG4bKabc".to_string());
        let ext = read_via_extension(&via, "INVITE", Some(&k)).expect("a verified extension");
        assert_eq!(ext.leg, "b-1");
        assert_eq!(ext.request_uri, "sip:bob@10.0.0.2:5070");
        assert!(ext.offer);
    }

    #[test]
    fn an_extension_is_trusted_under_the_signing_key_only() {
        let k = key(1);
        let o = StackIdentityOpts { request: Some((facts("sip:bob@10.0.0.2"), &k)), ..opts(false) };
        let via = build_call_via(&o, "z9hG4bKabc".to_string());
        assert_eq!(read_via_extension(&via, "INVITE", Some(&key(2))), None);
        assert_eq!(read_via_extension(&via, "INVITE", None), None);
    }

    #[test]
    fn an_extension_moved_to_another_call_fails_its_signature() {
        let k = key(1);
        let o = StackIdentityOpts { request: Some((facts("sip:bob@10.0.0.2"), &k)), ..opts(false) };
        let signed = build_call_via(&o, "z9hG4bKabc".to_string());
        let value = signed.param(via_extension::PARAM).cloned().expect("vx");
        let other = StackIdentityOpts { call_ref: "w1|other|tag", ..opts(false) };
        let moved = build_call_via(&other, "z9hG4bKabc".to_string())
            .with_param(via_extension::PARAM, value);
        assert_eq!(read_via_extension(&moved, "INVITE", Some(&k)), None);
    }

    #[test]
    fn an_extension_is_bound_to_its_branch_method_leg_and_incarnation() {
        let k = key(1);
        let o = StackIdentityOpts { request: Some((facts("sip:bob@10.0.0.2"), &k)), ..opts(false) };
        let signed = build_call_via(&o, "z9hG4bKabc".to_string());
        assert!(read_via_extension(&signed, "INVITE", Some(&k)).is_some());
        assert_eq!(read_via_extension(&signed, "BYE", Some(&k)), None, "another method");
        let value = signed.param(via_extension::PARAM).cloned().expect("vx");
        for other in [
            StackIdentityOpts { leg: "b-2", ..opts(false) },
            StackIdentityOpts { incarnation_mark: "other", ..opts(false) },
        ] {
            let moved = build_call_via(&other, "z9hG4bKabc".to_string())
                .with_param(via_extension::PARAM, value.clone());
            assert_eq!(read_via_extension(&moved, "INVITE", Some(&k)), None);
        }
        let rebranched = build_call_via(&opts(false), "z9hG4bKother".to_string())
            .with_param(via_extension::PARAM, value);
        assert_eq!(read_via_extension(&rebranched, "INVITE", Some(&k)), None, "another branch");
    }

    #[test]
    fn a_via_without_a_request_states_no_extension() {
        let via = build_call_via(&opts(false), "z9hG4bKabc".to_string());
        assert_eq!(via.param(via_extension::PARAM), None);
    }

    fn opts(is_emergency: bool) -> StackIdentityOpts<'static> {
        StackIdentityOpts {
            local_ip: "10.0.0.1",
            local_port: 5060,
            call_ref: "w0|cid|tag",
            leg: "b-1",
            incarnation_mark: "k3x9",
            is_emergency,
            other_owner: None,
            request: None,
        }
    }

    fn contact_facts(is_emergency: bool) -> ContactFacts<'static> {
        ContactFacts {
            host: "10.0.0.1",
            port: 5060,
            call_ref: "w0|cid|tag",
            leg: "b-1",
            incarnation_mark: "k3x9",
            is_emergency,
        }
    }

    /// Parameter names in wire order.
    fn param_names(params: &sip_message::header::Params) -> Vec<&str> {
        params.iter().map(|(name, _)| name.as_str()).collect()
    }

    // --- emergency stack-identity markers ---------------------------------
    // `build_call_via` appends `;em=1` and `build_call_contact` appends
    // `;emerg=1` iff the call is an emergency, so every in-dialog packet of an
    // admitted emergency call is identifiable on the wire.

    #[test]
    fn via_appends_em_marker_when_emergency() {
        let via = build_call_via(&opts(true), "z9hG4bKabc".to_string());
        // `em` rides *after* cr/lg/rport, value "1".
        assert_eq!(via.param("em").and_then(ParamValue::as_str), Some("1"));
        assert_eq!(param_names(via.params()), vec!["branch", "cr", "lg", "ci", "rport", "em"]);
    }

    #[test]
    fn via_omits_em_marker_when_not_emergency() {
        let via = build_call_via(&opts(false), "z9hG4bKabc".to_string());
        assert_eq!(via.param("em"), None);
    }

    #[test]
    fn contact_appends_emerg_marker_when_emergency() {
        let contact = build_call_contact(&contact_facts(true));
        assert_eq!(contact.param("emerg").and_then(ParamValue::as_str), Some("1"));
        assert_eq!(param_names(contact.params()), vec!["callRef", "leg", "ci", "emerg"]);
    }

    #[test]
    fn contact_omits_emerg_marker_when_not_emergency() {
        let contact = build_call_contact(&contact_facts(false));
        assert_eq!(contact.param("emerg"), None);
        assert_eq!(param_names(contact.params()), vec!["callRef", "leg", "ci"]);
    }

    // --- StackIdentity public read-API ------------------------------------
    // Port of `tests/b2bua/stack-identity-public-api.test.ts`
    // (describe "StackIdentity public read-API"). Pins the read-side seam
    // consumers use to populate their own templating layer ($(ip.AS) /
    // $(port.AS) substitution): the advertised host/port read back the
    // configured `B2buaConfig.sip_local_{ip,port}` (the source's
    // `AppConfig.sipLocalIp` / `sipLocalPort`).

    #[test]
    fn advertised_host_port_match_the_configured_values() {
        let cfg = B2buaConfig {
            sip_local_ip: "10.20.30.40".to_string(),
            sip_local_port: 35060,
            ..B2buaConfig::default()
        };
        let identity = StackIdentity::from_config(&cfg);
        assert_eq!(identity.advertised_host(), "10.20.30.40");
        assert_eq!(identity.advertised_port(), 35060);
    }

    #[test]
    fn default_config_is_reachable_end_to_end() {
        // The TS analogue of "default (testAppConfigDefaults) is reachable":
        // a default-config identity yields a non-empty host and a port.
        let identity = StackIdentity::from_config(&B2buaConfig::default());
        assert!(!identity.advertised_host().is_empty());
        assert!(identity.advertised_port() > 0);
    }
}
