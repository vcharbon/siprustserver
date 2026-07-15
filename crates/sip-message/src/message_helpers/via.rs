//! Via header value readers: the transaction `branch` plus the B2BUA's
//! custom correlation params `cr`/`lg` (stamped percent-encoded — see
//! [`super::param_codec`]).

use crate::parser::custom::structured_headers::parse_via;
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
    let parsed = parse_via(via_value);
    let pick = |key: &str| match parsed.params.get(key) {
        Some(ParamValue::Value(v)) => Some(v.clone()),
        _ => None,
    };
    ViaParams { branch: parsed.branch, cr: pick("cr"), lg: pick("lg") }
}
