//! Via header value readers and rewriters: the transaction `branch`, the
//! B2BUA's custom correlation params `cr`/`lg` (stamped percent-encoded —
//! see [`super::param_codec`]), and receive-side `received=`/`rport=`
//! stamping. Building a *new* Via value lives in [`crate::generators`].

use crate::parser::custom::structured_headers::parse_via;
use crate::sip_str::SipStr;
use crate::types::ParamValue;

/// The B2BUA's custom Via parameters: `branch`, `cr`, `lg`. Zero-regex.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ViaParams {
    pub branch: Option<String>,
    pub cr: Option<String>,
    pub lg: Option<String>,
}

/// Read `branch`/`cr`/`lg` from a Via header value. Values are returned as
/// written on the wire (still percent-encoded).
pub fn parse_via_params(via_value: &str) -> ViaParams {
    let parsed = parse_via(&SipStr::owned(via_value));
    let pick = |key: &str| match parsed.params.get(key) {
        Some(ParamValue::Value(v)) => Some(v.to_string()),
        _ => None,
    };
    ViaParams { branch: parsed.branch.map(String::from), cr: pick("cr"), lg: pick("lg") }
}

/// The sent-by `(host, port)` of a Via header value (RFC 3261 §18.2.2 —
/// where a response to the request carrying this Via is sent). Port defaults
/// to 5060 when absent; `None` when the value carries no parseable host.
pub fn via_sent_by(via_value: &str) -> Option<(String, u64)> {
    let parsed = parse_via(&SipStr::owned(via_value));
    if parsed.host.is_empty() {
        return None;
    }
    Some((parsed.host.into(), parsed.port.unwrap_or(5060)))
}

/// RFC 3261 §18.2.1 + RFC 3581 §4: stamp `received=` (if sent-by host differs
/// from the source) and replace any `rport` flag with `rport=<port>` on a
/// single Via entry value. Idempotent: already-populated parameters are left
/// alone.
pub fn stamp_received_rport_on_via(value: &str, src_ip: &str, src_port: u16) -> String {
    let (head, mut params) = match value.find(';') {
        Some(semi) => (&value[..semi], value[semi..].to_string()),
        None => (value, String::new()),
    };
    let hp = head.split(' ').next_back().unwrap_or("");
    let sent_by_host = match hp.rfind(':') {
        Some(colon) => &hp[..colon],
        None => hp,
    };
    let need_received = sent_by_host != src_ip;
    let lower = params.to_ascii_lowercase();
    let has_received = lower.contains(";received=");
    let rport_flag = find_rport_flag(&params).is_some();

    if need_received && !has_received {
        params.push_str(&format!(";received={src_ip}"));
    }
    if rport_flag {
        params = replace_rport_flag(&params, src_port);
    }
    format!("{head}{params}")
}

/// Find the byte offset of a bare `;rport` flag (followed by `;` or end,
/// case-insensitive) in `params`, if present.
fn find_rport_flag(params: &str) -> Option<usize> {
    let lower = params.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut search_from = 0;
    while let Some(rel) = lower[search_from..].find(";rport") {
        let idx = search_from + rel;
        let after = idx + ";rport".len();
        if after == bytes.len() || bytes[after] == b';' {
            return Some(idx);
        }
        search_from = after;
    }
    None
}

fn replace_rport_flag(params: &str, src_port: u16) -> String {
    match find_rport_flag(params) {
        Some(idx) => {
            let after = idx + ";rport".len();
            format!("{};rport={}{}", &params[..idx], src_port, &params[after..])
        }
        None => params.to_string(),
    }
}
