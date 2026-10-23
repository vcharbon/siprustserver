//! Per-UA Record-Route folding: whether a simulated UAS echoes multiple
//! Record-Route rows as separate lines or comma-combined into one (RFC 3261
//! §7.3.1 permits both wire forms).

use sip_message::header::HeaderName;
use sip_message::SipHeader;

/// How a simulated UA, acting as UAS, echoes multiple Record-Route header rows
/// back in its response: as separate `Record-Route:` lines, or folded into a
/// single comma-separated list. RFC 3261 §7.3.1 permits the combined form, and
/// real UAs (SIPp, many phones) emit it — so the b-leg 200 OK can carry the front
/// proxy's *double*-record-route halves comma-combined in ONE header, which the
/// B2BUA must split before the §12.1.2 route-set reverse (the long-call-loss
/// class — see `b2bua/src/rules/actions.rs`). The harness picks this per-UA at
/// bind time so a run exercises both wire forms.
///
/// NOTE: this only has an observable effect when a response echoes ≥ 2
/// Record-Route headers, which in practice means the *real* double-record-routing
/// `sip-proxy` (failover-harness). The harness's own loose-routing
/// [`Proxy`](super::Proxy) inserts a single `;lr` Record-Route, so folding is a
/// no-op there and the deterministic report bytes of peer-to-peer scenarios are
/// unaffected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordRouteFold {
    /// One `Record-Route:` line per route (the strict form).
    Separate,
    /// All Record-Route rows folded into one comma-separated header (§7.3.1).
    Combined,
}

/// Decide a UA's Record-Route fold mode. `HARNESS_RR_FOLD=separate|combined`
/// pins it; otherwise it is a coin flip keyed on the UA name alone (FNV-1a), so
/// every run, every process and every replay of a cell folds alike, and a run
/// still exercises both wire forms across its UAs.
pub(crate) fn decide_rr_fold(name: &str) -> RecordRouteFold {
    match std::env::var("HARNESS_RR_FOLD").ok().as_deref() {
        Some("separate") => RecordRouteFold::Separate,
        Some("combined") => RecordRouteFold::Combined,
        _ => {
            let hash = name.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
                (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
            });
            if hash & 1 == 0 {
                RecordRouteFold::Separate
            } else {
                RecordRouteFold::Combined
            }
        }
    }
}

/// Fold every Record-Route header in `headers` into a single comma-separated
/// header at the position of the first (RFC 3261 §7.3.1). No-op for < 2 rows.
pub(super) fn fold_record_routes(headers: &mut Vec<SipHeader>) {
    let idxs: Vec<usize> = headers
        .iter()
        .enumerate()
        .filter(|(_, h)| HeaderName::RecordRoute.matches(&h.name))
        .map(|(i, _)| i)
        .collect();
    if idxs.len() < 2 {
        return;
    }
    let combined = idxs.iter().map(|&i| headers[i].value.clone()).collect::<Vec<_>>().join(", ");
    headers[idxs[0]].value = combined.into();
    for &i in idxs[1..].iter().rev() {
        headers.remove(i);
    }
}
