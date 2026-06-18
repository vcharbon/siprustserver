//! Leg "stack identity" — port of `src/b2bua/stack-identity.ts`. Stamps the
//! B2BUA's own Via + Contact onto outbound messages, carrying the `callRef`
//! (`cr`) and leg id (`lg`) in opaque params so any inbound response/request
//! self-routes back to the owning call without an external lookup.
//!
//! `callRef` contains `|` (and Call-IDs may contain `@`/`:`), which are unsafe
//! in a SIP param value, so the values are percent-encoded here and decoded by
//! [`decode_param`] on the read path.

use crate::config::B2buaConfig;
use sip_message::generators::{ContactSpec, SipTransport, ViaSpec};
// The cr/lg/callRef param codec lives in sip-message so the encoder and its
// inverse can't drift across crates; re-export so existing `stack_identity::`
// callers (the router's read path) are unchanged.
pub use sip_message::message_helpers::{decode_param, encode_param};

/// Inputs shared by the Via + Contact builders.
pub struct StackIdentityOpts<'a> {
    pub local_ip: &'a str,
    pub local_port: u16,
    pub call_ref: &'a str,
    pub leg: &'a str,
    pub is_emergency: bool,
}

/// Build the B2BUA Via for an outbound message (with `cr`/`lg`/`rport` params).
pub fn build_call_via(opts: &StackIdentityOpts, branch: String) -> ViaSpec {
    let mut custom_params = vec![
        ("cr".to_string(), encode_param(opts.call_ref)),
        ("lg".to_string(), encode_param(opts.leg)),
        ("rport".to_string(), String::new()),
    ];
    if opts.is_emergency {
        custom_params.push(("em".to_string(), "1".to_string()));
    }
    ViaSpec {
        local_ip: opts.local_ip.to_string(),
        local_port: opts.local_port,
        transport: SipTransport::Udp,
        branch,
        custom_params,
    }
}

/// Build the B2BUA Contact for an outbound message (with `callRef`/`leg` params).
pub fn build_call_contact(opts: &StackIdentityOpts) -> ContactSpec {
    let mut uri_params = vec![
        ("callRef".to_string(), encode_param(opts.call_ref)),
        ("leg".to_string(), encode_param(opts.leg)),
    ];
    if opts.is_emergency {
        uri_params.push(("emerg".to_string(), "1".to_string()));
    }
    ContactSpec {
        user: "b2bua".to_string(),
        host: opts.local_ip.to_string(),
        port: opts.local_port,
        uri_params,
    }
}

/// Convenience: both the Via and Contact for a single outbound hop (port of
/// `buildCallViaAndContact`). The Contact ignores `branch`, so callers that need
/// only one of the two should call the single builders directly.
pub fn build_call_via_and_contact(
    opts: &StackIdentityOpts,
    branch: String,
) -> (ViaSpec, ContactSpec) {
    (build_call_via(opts, branch), build_call_contact(opts))
}

// ---------------------------------------------------------------------------
// Public read-side seam — consumer-facing API for the addresses the B2BUA
// stamps on outbound Contact / Via (port of `StackIdentity` /
// `StackIdentityApi`, Issue 8 of the upstream-consumer plan).
// ---------------------------------------------------------------------------

/// Read-only view of the addresses this B2BUA advertises to its peers.
/// Consumers running their own templating layer (e.g. resolving `$(ip.AS)` /
/// `$(port.AS)` placeholders in their call-control payloads) read these once at
/// startup, then hand fully-resolved literals to the decision engine.
///
/// In the TS source these are `Effect<string>` / `Effect<number>` reads behind a
/// `ServiceMap.Service` DI seam; here the read channel is `never` (a pure config
/// projection), so the faithful Rust idiom is a cheap value with accessors —
/// mirroring `sip_proxy`'s `ProxyCore::advertised()`. The `Default` layer is
/// replaced by [`StackIdentity::from_config`], deriving the advertised
/// host/port from [`B2buaConfig::sip_local_ip`] / [`B2buaConfig::sip_local_port`]
/// (the `AppConfig.sipLocalIp` / `sipLocalPort` of the source). If a separate
/// "advertised" address slot is ever added, the accessor names stay the same.
///
/// This is a **forward-looking consumer seam**: it mirrors the TS public
/// read-API one-for-one but has no in-tree caller yet (the outbound builders
/// above read the same fields straight off [`B2buaConfig`]). It exists so a
/// consumer running its own `$(ip.AS)` / `$(port.AS)` templating layer has a
/// stable contract to read once at startup; do not delete it as dead code.
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

    /// Host the B2BUA stamps on outbound Contact and Via (`advertisedHost`).
    /// Today this maps to [`B2buaConfig::sip_local_ip`].
    pub fn advertised_host(&self) -> &str {
        &self.advertised_host
    }

    /// Port the B2BUA stamps on outbound Contact and Via (`advertisedPort`).
    /// Today this maps to [`B2buaConfig::sip_local_port`].
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
    fn via_carries_cr_lg_rport() {
        let opts = StackIdentityOpts {
            local_ip: "10.0.0.1",
            local_port: 5060,
            call_ref: "w0|cid|tag",
            leg: "b-1",
            is_emergency: false,
        };
        let via = build_call_via(&opts, "z9hG4bKabc".to_string());
        let names: Vec<&str> = via.custom_params.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["cr", "lg", "rport"]);
        assert_eq!(via.branch, "z9hG4bKabc");
    }

    fn opts(is_emergency: bool) -> StackIdentityOpts<'static> {
        StackIdentityOpts {
            local_ip: "10.0.0.1",
            local_port: 5060,
            call_ref: "w0|cid|tag",
            leg: "b-1",
            is_emergency,
        }
    }

    fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
        params
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    // --- emergency stack-identity markers (the subject of this slice) ------
    // `buildCallVia` appends `;em=1` and `buildCallContact` appends `;emerg=1`
    // iff `opts.isEmergency` (stack-identity.ts L42 / L58). These are the
    // in-dialog markers `bufferHasEmergencyMarker` scans for on the read side.

    #[test]
    fn via_appends_em_marker_when_emergency() {
        let via = build_call_via(&opts(true), "z9hG4bKabc".to_string());
        // `em` rides *after* cr/lg/rport, value "1".
        assert_eq!(param(&via.custom_params, "em"), Some("1"));
        let names: Vec<&str> = via.custom_params.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["cr", "lg", "rport", "em"]);
    }

    #[test]
    fn via_omits_em_marker_when_not_emergency() {
        let via = build_call_via(&opts(false), "z9hG4bKabc".to_string());
        assert_eq!(param(&via.custom_params, "em"), None);
    }

    #[test]
    fn contact_appends_emerg_marker_when_emergency() {
        let contact = build_call_contact(&opts(true));
        assert_eq!(param(&contact.uri_params, "emerg"), Some("1"));
        let names: Vec<&str> = contact.uri_params.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["callRef", "leg", "emerg"]);
    }

    #[test]
    fn contact_omits_emerg_marker_when_not_emergency() {
        let contact = build_call_contact(&opts(false));
        assert_eq!(param(&contact.uri_params, "emerg"), None);
        let names: Vec<&str> = contact.uri_params.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["callRef", "leg"]);
    }

    #[test]
    fn via_and_contact_convenience_agrees_with_single_builders() {
        let o = opts(true);
        let (via, contact) = build_call_via_and_contact(&o, "z9hG4bKxyz".to_string());
        assert_eq!(via, build_call_via(&o, "z9hG4bKxyz".to_string()));
        assert_eq!(contact, build_call_contact(&o));
        // Both carry the emergency marker for an emergency hop.
        assert_eq!(param(&via.custom_params, "em"), Some("1"));
        assert_eq!(param(&contact.uri_params, "emerg"), Some("1"));
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
