//! The stickiness cookie read back: its fields present, of this version, and
//! signed for the call it rides on. A cookie that passes is one this strategy
//! minted for that call; the routing over the registry is `decode_stickiness`'s.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use sip_message::SipMessage;

use crate::observability::metrics::HmacFailureReason;
use crate::security::hmac::TRUNCATED_MAC_BYTES;
use crate::strategy::{DecodeResult, RouteParams};

use super::{build_stickiness_input, call_id_of, LoadBalancerStrategy, COOKIE_VERSION};

/// Every field of the cookie, as `encode_stickiness` writes them.
pub(super) const FIELDS: [&str; 6] = ["w_pri", "w_bak", "e", "v", "kid", "sig"];

/// A cookie whose signature verified.
pub(super) struct Checked<'p> {
    pub(super) w_pri: &'p str,
    pub(super) w_bak: &'p str,
    pub(super) is_emergency: bool,
}

/// Why a cookie did not verify.
pub(super) enum Fault {
    /// A field is absent, empty where it must not be, or `e` is not 0/1.
    Missing,
    /// Another cookie version, a message with no Call-ID, or an unreadable
    /// signature.
    Unreadable(String),
    /// The signature does not match the fields.
    Mismatch,
}

impl Fault {
    pub(super) fn metric(&self) -> HmacFailureReason {
        match self {
            Fault::Missing => HmacFailureReason::Missing,
            Fault::Unreadable(_) => HmacFailureReason::Decode,
            Fault::Mismatch => HmacFailureReason::Mismatch,
        }
    }

    /// What a request routed by a cookie that failed this way gets.
    pub(super) fn decode_result(self) -> DecodeResult {
        match self {
            Fault::Missing => DecodeResult::Unknown { is_emergency: false },
            Fault::Unreadable(reason) => DecodeResult::Reject { status: 403, reason },
            Fault::Mismatch => DecodeResult::Reject {
                status: 403,
                reason: "stickiness signature mismatch".to_string(),
            },
        }
    }
}

impl LoadBalancerStrategy {
    /// Read and verify the cookie in `params` for `msg`'s call.
    pub(super) fn check<'p>(
        &self,
        params: &'p RouteParams,
        msg: &SipMessage,
    ) -> Result<Checked<'p>, Fault> {
        let [w_pri, w_bak, e, v, kid, sig] = FIELDS.map(|f| params.get(f).map(String::as_str));
        // `w_bak` may legitimately be present-but-empty; `w_pri`/`kid`/`sig` must
        // be non-empty; `e` must be "0" or "1".
        let (Some(w_pri), Some(w_bak), Some(e), Some(v), Some(kid), Some(sig)) =
            (w_pri, w_bak, e, v, kid, sig)
        else {
            return Err(Fault::Missing);
        };
        if (e != "0" && e != "1") || w_pri.is_empty() || kid.is_empty() || sig.is_empty() {
            return Err(Fault::Missing);
        }
        if v != COOKIE_VERSION {
            return Err(Fault::Unreadable(format!(
                "unsupported stickiness cookie version \"{v}\""
            )));
        }
        let Some(call_id) = call_id_of(msg) else {
            return Err(Fault::Unreadable("missing Call-ID for stickiness verify".to_string()));
        };
        let Ok(decoded) = URL_SAFE_NO_PAD.decode(sig) else {
            return Err(Fault::Unreadable("malformed stickiness signature".to_string()));
        };
        if decoded.len() != TRUNCATED_MAC_BYTES {
            return Err(Fault::Unreadable(format!(
                "malformed stickiness signature (length={})",
                decoded.len()
            )));
        }
        let input = build_stickiness_input(w_pri, w_bak, e, call_id);
        if !self.hmac.verify_truncated(&input, kid, &decoded) {
            return Err(Fault::Mismatch);
        }
        Ok(Checked { w_pri, w_bak, is_emergency: e == "1" })
    }

    /// The cookie in `params` restricted to [`FIELDS`], when it verifies.
    pub(super) fn own_fields(&self, params: &RouteParams, msg: &SipMessage) -> Option<RouteParams> {
        self.check(params, msg).ok()?;
        Some(
            FIELDS
                .iter()
                .filter_map(|f| params.get(*f).map(|value| ((*f).to_string(), value.clone())))
                .collect(),
        )
    }
}
