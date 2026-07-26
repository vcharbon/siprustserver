//! The `X-Overload` header-value codec — the `(elu, gc, adm)` triple workers
//! publish on OPTIONS 200 replies. Only the *value* is parsed here; pulling the
//! header off a message is sip-message's job (`msg.raw(X_OVERLOAD)`).
//! The emit side lives in the worker (`b2bua::overload`).

use std::collections::HashMap;

use sip_message::sip_str::SipStr;
use sip_message::HeaderName;

/// The extension header workers publish their overload triple on.
pub const X_OVERLOAD: HeaderName = HeaderName::Other(SipStr::from_static("X-Overload"));

/// The `(elu, gc, adm)` triple parsed off an `X-Overload` header.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OverloadPayload {
    /// Event-loop utilization EWMA, clamped to `0..=1`.
    pub elu: f64,
    /// GC pause fraction EWMA, clamped to `0..=1`.
    pub gc: f64,
    /// Worker's monotonic counter of non-emergency new-dialog admits (`>= 0`).
    pub adm: f64,
}

/// Parse a worker's `X-Overload: v=1; elu=…; gc=…; adm=…` header value. Returns
/// `None` for missing/malformed/unknown-version headers (callers tick a
/// `payload_missing` counter on a miss). Forward-compatible: `v` other than `1`
/// → `None`; unknown params ignored.
pub fn parse_x_overload_header(value: Option<&str>) -> Option<OverloadPayload> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    let mut params: HashMap<&str, &str> = HashMap::new();
    for segment in value.split(';') {
        let trimmed = segment.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((k, v)) = trimmed.split_once('=') {
            params.insert(k.trim(), v.trim());
        }
    }
    if params.get("v") != Some(&"1") {
        return None;
    }
    let elu: f64 = params.get("elu")?.parse().ok()?;
    let gc: f64 = params.get("gc")?.parse().ok()?;
    let adm: f64 = params.get("adm")?.parse().ok()?;
    if !elu.is_finite() || !gc.is_finite() || !adm.is_finite() || adm < 0.0 {
        return None;
    }
    let clamp01 = |n: f64| n.clamp(0.0, 1.0);
    Some(OverloadPayload { elu: clamp01(elu), gc: clamp01(gc), adm })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_x_overload_header() {
        let p = parse_x_overload_header(Some("v=1; elu=0.5; gc=0.1; adm=42")).unwrap();
        assert_eq!(p.elu, 0.5);
        assert_eq!(p.gc, 0.1);
        assert_eq!(p.adm, 42.0);
    }

    #[test]
    fn rejects_bad_overload_headers() {
        assert!(parse_x_overload_header(None).is_none());
        assert!(parse_x_overload_header(Some("")).is_none());
        assert!(parse_x_overload_header(Some("v=2; elu=0.5; gc=0; adm=0")).is_none());
        assert!(parse_x_overload_header(Some("v=1; elu=x; gc=0; adm=0")).is_none());
        assert!(parse_x_overload_header(Some("v=1; elu=0.5; gc=0; adm=-1")).is_none());
    }

    #[test]
    fn clamps_elu_and_gc() {
        let p = parse_x_overload_header(Some("v=1; elu=1.5; gc=-0.2; adm=3")).unwrap();
        assert_eq!(p.elu, 1.0);
        assert_eq!(p.gc, 0.0);
    }
}
