//! Readers over From/To/Contact header *values* (name-addr forms): tag
//! extraction/stripping and URI extraction. Quote-aware via the
//! structured-header parser.

use crate::parser::custom::structured_headers::{parse_contact, parse_name_addr};
use crate::types::ParamValue;

/// Extract the `tag` parameter from a From/To header value.
pub fn extract_tag(header_value: &str) -> Option<String> {
    parse_name_addr(header_value).tag
}

/// Strip the `tag` parameter from a From/To header value, reconstructing the
/// remaining name-addr + params.
pub fn strip_tag(header_value: &str) -> String {
    let parsed = parse_name_addr(header_value);
    if parsed.tag.is_none() {
        return header_value.to_string();
    }

    let mut result = String::new();
    if let Some(dn) = &parsed.display_name {
        result.push_str(&format!("\"{dn}\" "));
    }
    result.push_str(&format!("<{}>", parsed.uri));
    for (k, v) in &parsed.params {
        if k == "tag" {
            continue;
        }
        match v {
            ParamValue::Flag => result.push_str(&format!(";{k}")),
            ParamValue::Value(val) => result.push_str(&format!(";{k}={val}")),
        }
    }
    result
}

/// Extract the URI from a From/To header value (name-addr).
pub fn extract_name_addr_uri(header_value: &str) -> String {
    parse_name_addr(header_value).uri
}

/// Extract the URI from a Contact header value.
pub fn extract_contact_uri(contact_value: &str) -> String {
    parse_contact(contact_value).uri
}
