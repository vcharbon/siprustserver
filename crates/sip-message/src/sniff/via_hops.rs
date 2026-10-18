//! What a datagram's Via rows say about its hops: the top Via's `rport`
//! (RFC 3581) and the originating hop at the bottom (RFC 3261 §16.6).

use crate::header::{HeaderValue, ParamValue, Rport, SentBy, Via};
use crate::sip_str::SipStr;

use super::{as_str, header_rows_compact};

/// What the TOP-most Via says about `rport` (RFC 3581): the four states a
/// server's echo can be in, kept apart because the obligation differs — a bare
/// request asks, and a response owes the observed port.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViaRport {
    /// No `rport` parameter at all.
    Absent,
    /// `;rport` bare — the sender asking to be told its source port.
    Requested,
    /// `;rport=<port>` — the port the next hop observed.
    Observed(u16),
    /// `rport` is there carrying a value no reader accepts.
    Unreadable,
}

/// The `rport` parameter of the TOP-most Via (RFC 3581 §4). Only the first Via
/// matters: it is the hop whose response routing `rport` fixes.
pub fn via_rport(raw: &[u8]) -> ViaRport {
    for line in as_str(raw).lines() {
        if line.is_empty() {
            break; // end of headers
        }
        let Some((h, v)) = line.split_once(':') else { continue };
        let h = h.trim();
        if !(h.eq_ignore_ascii_case("via") || h.eq_ignore_ascii_case("v")) {
            continue;
        }
        let Ok(vias) = Via::parse_line(&SipStr::owned(v.trim())) else {
            return ViaRport::Unreadable;
        };
        let Some(top) = vias.first() else { return ViaRport::Unreadable };
        return match (top.param("rport"), top.rport()) {
            (None, _) => ViaRport::Absent,
            (Some(ParamValue::Flag), _) => ViaRport::Requested,
            (Some(_), Some(Rport::Observed(port))) => ViaRport::Observed(port),
            (Some(_), _) => ViaRport::Unreadable,
        };
    }
    ViaRport::Absent
}

/// The originating hop of a request, read off its BOTTOM-most Via: every
/// proxy pushes its own Via above the ones it received (RFC 3261 §16.6 step 8),
/// so the bottom one is the originator's as it sent the request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OriginVia {
    /// The bottom Via's sent-by: the originator's, as it sent the request.
    pub sent_by: SentBy,
    /// The bottom Via's `branch`, or `None` where it carries none.
    pub branch: Option<String>,
    /// How many Via values the request carried: one per hop it crossed.
    pub depth: usize,
}

/// The bottom-most Via of `raw` as an [`OriginVia`], or `None` where the
/// message carries no Via or one of its Via rows does not parse.
pub fn bottom_via(raw: &[u8]) -> Option<OriginVia> {
    let mut depth = 0;
    let mut bottom = None;
    for row in header_rows_compact(raw, "via") {
        let vias = Via::parse_line(&SipStr::owned(&row)).ok()?;
        depth += vias.len();
        bottom = vias.last().cloned().or(bottom);
    }
    let bottom = bottom?;
    Some(OriginVia {
        sent_by: SentBy::of(&bottom),
        branch: bottom.branch().map(str::to_string),
        depth,
    })
}
