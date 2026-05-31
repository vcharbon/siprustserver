//! Leg "stack identity" — port of `src/b2bua/stack-identity.ts`. Stamps the
//! B2BUA's own Via + Contact onto outbound messages, carrying the `callRef`
//! (`cr`) and leg id (`lg`) in opaque params so any inbound response/request
//! self-routes back to the owning call without an external lookup.
//!
//! `callRef` contains `|` (and Call-IDs may contain `@`/`:`), which are unsafe
//! in a SIP param value, so the values are percent-encoded here and decoded by
//! [`decode_param`] on the read path.

use sip_message::generators::{ContactSpec, ViaSpec, SipTransport};

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

/// Percent-encode all but RFC 3986 unreserved characters.
pub fn encode_param(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~');
        if unreserved {
            out.push(b as char);
        } else {
            out.push('%');
            out.push(hex_digit(b >> 4));
            out.push(hex_digit(b & 0x0f));
        }
    }
    out
}

/// Inverse of [`encode_param`]; invalid escapes are passed through verbatim.
pub fn decode_param(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(v: u8) -> char {
    match v {
        0..=9 => (b'0' + v) as char,
        _ => (b'a' + (v - 10)) as char,
    }
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
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
}
