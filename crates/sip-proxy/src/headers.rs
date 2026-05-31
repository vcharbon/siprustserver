//! Proxy header surgery — the byte-level Via / Record-Route / Route operations
//! the stateless proxy performs per RFC 3261 §16. These are *proxy policy*
//! composers built on `sip-message`'s generic parsing primitives
//! (`parse_sip_uri`, `first_route_is_loose`, `strip_route_uri_to_request_uri`);
//! they live here rather than in `sip-message` until a second consumer needs
//! them (ADR-0002).
//!
//! Everything operates on the raw `Vec<SipHeader>` wire-order list (the proxy
//! re-serializes after surgery), matching the source `ProxyCore`'s direct
//! header-stack manipulation.

use sip_message::generators::strip_route_uri_to_request_uri;
use sip_message::message_helpers::parse_sip_uri;
use sip_message::types::SipHeader;

use crate::addr::ProxyAddr;

/// Insert a header at the top of the list — RFC 3261 §16.6 prepend semantics for
/// Via / Record-Route.
pub fn prepend_header(headers: &mut Vec<SipHeader>, name: &str, value: &str) {
    headers.insert(0, SipHeader { name: name.to_string(), value: value.to_string() });
}

/// Replace the value of the first header named `name` (case-insensitive), or
/// append it if absent (upsert). Used for Max-Forwards.
pub fn upsert_header(headers: &mut Vec<SipHeader>, name: &str, value: &str) {
    if let Some(h) = headers.iter_mut().find(|h| h.name.eq_ignore_ascii_case(name)) {
        h.value = value.to_string();
    } else {
        headers.push(SipHeader { name: name.to_string(), value: value.to_string() });
    }
}

/// The value of the first header named `name` (case-insensitive).
pub fn first_header_value<'a>(headers: &'a [SipHeader], name: &str) -> Option<&'a str> {
    headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

/// Remove the first header named `name` (case-insensitive) entirely; returns its
/// value if one was removed.
pub fn remove_first_header(headers: &mut Vec<SipHeader>, name: &str) -> Option<String> {
    let pos = headers.iter().position(|h| h.name.eq_ignore_ascii_case(name))?;
    Some(headers.remove(pos).value)
}

/// Pop the **first entry** of the first header named `name`, honouring
/// comma-combined values (RFC 3261 §7.3.1 — multiple Via hops can share one
/// header line). If the first matching header carries a single entry, the whole
/// header is removed; if it carries `a, b, c`, only `a` is dropped and the
/// header keeps `b, c`. Returns the popped entry's value.
pub fn remove_first_header_entry(headers: &mut Vec<SipHeader>, name: &str) -> Option<String> {
    let pos = headers.iter().position(|h| h.name.eq_ignore_ascii_case(name))?;
    let value = headers[pos].value.clone();
    match split_top_level_commas(&value).split_first() {
        Some((first, rest)) if !rest.is_empty() => {
            headers[pos].value = rest.join(", ");
            Some(first.clone())
        }
        _ => {
            headers.remove(pos);
            Some(value)
        }
    }
}

/// Split a header value on top-level commas, ignoring commas inside `<...>`
/// (URI) and `"..."` (display name) — RFC 3261 §7.3.1.
pub fn split_top_level_commas(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut depth_angle = 0i32;
    let mut in_quote = false;
    let mut cur = String::new();
    for ch in value.chars() {
        match ch {
            '"' => {
                in_quote = !in_quote;
                cur.push(ch);
            }
            '<' if !in_quote => {
                depth_angle += 1;
                cur.push(ch);
            }
            '>' if !in_quote => {
                depth_angle -= 1;
                cur.push(ch);
            }
            ',' if !in_quote && depth_angle == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(ch),
        }
    }
    let last = cur.trim();
    if !last.is_empty() || out.is_empty() {
        out.push(last.to_string());
    }
    out
}

/// RFC 3261 §18.2.1 + RFC 3581 §4: on the topmost Via, stamp `received=<ip>` (if
/// the sent-by host differs from the actual source) and replace a bare `;rport`
/// flag with `;rport=<port>`. Idempotent. Operates on the first Via entry
/// (comma-aware: rewrites only the leading hop of a combined line).
pub fn populate_received_rport_on_top_via(headers: &mut [SipHeader], src_ip: &str, src_port: u16) {
    let Some(h) = headers.iter_mut().find(|h| h.name.eq_ignore_ascii_case("via")) else {
        return;
    };
    let entries = split_top_level_commas(&h.value);
    let Some((first, rest)) = entries.split_first() else {
        return;
    };
    let stamped = stamp_received_rport(first, src_ip, src_port);
    if rest.is_empty() {
        h.value = stamped;
    } else {
        let mut joined = vec![stamped];
        joined.extend(rest.iter().cloned());
        h.value = joined.join(", ");
    }
}

/// Build a loose-route Record-Route value: `<sip:host:port;k=v;...;lr>`. The
/// stickiness params (deterministically ordered by the `BTreeMap`) precede the
/// trailing `;lr`.
pub fn build_record_route_value<'a>(
    advertised: &ProxyAddr,
    params: impl IntoIterator<Item = (&'a String, &'a String)>,
) -> String {
    let mut uri = format!("<sip:{}:{}", advertised.host, advertised.port);
    for (k, v) in params {
        uri.push(';');
        uri.push_str(k);
        uri.push('=');
        uri.push_str(v);
    }
    uri.push_str(";lr>");
    uri
}

/// Parse the `host:port` of a Route/Record-Route URI value into a [`ProxyAddr`].
/// Defaults the port to 5060 when the URI omits it (RFC 3261 default).
pub fn route_value_to_addr(route_value: &str) -> Option<ProxyAddr> {
    let uri = strip_route_uri_to_request_uri(route_value);
    let parsed = parse_sip_uri(&uri)?;
    Some(ProxyAddr::new(parsed.host, parsed.port as u16))
}

/// True if `route_value`'s URI host:port is the proxy's advertised address.
pub fn route_targets_self(route_value: &str, advertised: &ProxyAddr) -> bool {
    route_value_to_addr(route_value).as_ref() == Some(advertised)
}

/// The sent-by `host:port` of a Via entry value (the token after the transport).
fn via_sent_by(via_entry: &str) -> Option<&str> {
    via_entry.split_whitespace().nth(1).and_then(|s| s.split(';').next()).map(str::trim)
}

/// True if a Via entry's sent-by is the proxy's advertised address.
pub fn via_entry_is_self(via_entry: &str, advertised: &ProxyAddr) -> bool {
    via_sent_by(via_entry)
        .and_then(ProxyAddr::parse)
        .map(|a| &a == advertised)
        .unwrap_or(false)
}

// --- received/rport stamping (port of generators::stamp_received_rport_on_via) ---

fn stamp_received_rport(value: &str, src_ip: &str, src_port: u16) -> String {
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
    if need_received && !has_received {
        params.push_str(&format!(";received={src_ip}"));
    }
    if let Some(idx) = find_rport_flag(&params) {
        let after = idx + ";rport".len();
        params = format!("{};rport={}{}", &params[..idx], src_port, &params[after..]);
    }
    format!("{head}{params}")
}

fn find_rport_flag(params: &str) -> Option<usize> {
    let lower = params.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    while let Some(rel) = lower[from..].find(";rport") {
        let idx = from + rel;
        let after = idx + ";rport".len();
        if after == bytes.len() || bytes[after] == b';' {
            return Some(idx);
        }
        from = after;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(name: &str, value: &str) -> SipHeader {
        SipHeader { name: name.to_string(), value: value.to_string() }
    }

    #[test]
    fn split_commas_respects_brackets_and_quotes() {
        let v = "\"Bob, the UA\" <sip:b@h;x=1,2>, SIP/2.0/UDP h2;branch=z";
        let parts = split_top_level_commas(v);
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1], "SIP/2.0/UDP h2;branch=z");
    }

    #[test]
    fn remove_first_via_entry_pops_only_top_of_combined_line() {
        let mut headers = vec![hdr("Via", "SIP/2.0/UDP a:5060;branch=z1, SIP/2.0/UDP b:5070;branch=z2")];
        let popped = remove_first_header_entry(&mut headers, "Via").unwrap();
        assert_eq!(popped, "SIP/2.0/UDP a:5060;branch=z1");
        assert_eq!(headers[0].value, "SIP/2.0/UDP b:5070;branch=z2");
    }

    #[test]
    fn remove_first_via_entry_removes_whole_single_header() {
        let mut headers = vec![hdr("Via", "SIP/2.0/UDP a:5060;branch=z1"), hdr("Via", "SIP/2.0/UDP b:5070;branch=z2")];
        let popped = remove_first_header_entry(&mut headers, "Via").unwrap();
        assert_eq!(popped, "SIP/2.0/UDP a:5060;branch=z1");
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].value, "SIP/2.0/UDP b:5070;branch=z2");
    }

    #[test]
    fn received_rport_stamped_when_host_differs_and_flag_present() {
        let mut headers = vec![hdr("Via", "SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK1;rport")];
        populate_received_rport_on_top_via(&mut headers, "203.0.113.5", 41234);
        assert!(headers[0].value.contains(";received=203.0.113.5"));
        assert!(headers[0].value.contains(";rport=41234"));
    }

    #[test]
    fn record_route_value_orders_params_then_lr() {
        let adv = ProxyAddr::new("10.0.0.1", 5060);
        let params: std::collections::BTreeMap<String, String> =
            [("v".to_string(), "3".to_string()), ("w_pri".to_string(), "b2b-1".to_string())].into_iter().collect();
        let rr = build_record_route_value(&adv, params.iter());
        assert_eq!(rr, "<sip:10.0.0.1:5060;v=3;w_pri=b2b-1;lr>");
    }

    #[test]
    fn route_targets_self_matches_advertised() {
        let adv = ProxyAddr::new("10.0.0.1", 5060);
        assert!(route_targets_self("<sip:10.0.0.1:5060;lr>", &adv));
        assert!(!route_targets_self("<sip:10.9.9.9:5060;lr>", &adv));
    }

    #[test]
    fn via_entry_self_detection() {
        let adv = ProxyAddr::new("10.0.0.1", 5060);
        assert!(via_entry_is_self("SIP/2.0/UDP 10.0.0.1:5060;branch=z", &adv));
        assert!(!via_entry_is_self("SIP/2.0/UDP 10.0.0.9:5060;branch=z", &adv));
    }
}
