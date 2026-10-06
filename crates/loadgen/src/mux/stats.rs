//! Process-wide mux counters + their Prometheus rendering.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use metric_catalogue::OpenRows;
use sip_message::sniff::{cseq_method_token, cseq_value, first_line};

use super::MuxCore;

/// Process-wide mux counters (Prometheus + report).
/// The orphan rows of `loadgen_mux_orphan_total`.
struct OrphanRows(OpenRows);

impl Default for OrphanRows {
    fn default() -> Self {
        Self(OpenRows::new(&crate::catalogue::MUX_ORPHAN))
    }
}

#[derive(Default)]
pub struct MuxStats {
    pub orphan_no_header: AtomicU64,
    pub orphan_unknown_token: AtomicU64,
    pub orphan_stray: AtomicU64,
    /// An initial INVITE on a Call-ID a finished call released within
    /// [`RELEASE_HOLD`](super::RELEASE_HOLD): a late retransmission of a leg of
    /// that call, never routed to the next call on its key.
    pub orphan_released: AtomicU64,
    /// A leg carrying a call's key before that call's caller sent its INVITE.
    pub orphan_early: AtomicU64,
    /// A new INVITE (To without a tag) on a Call-ID a caller owns: the SUT
    /// reused the dialog's Call-ID for a new leg (RFC 3261 §8.1.1.4).
    pub orphan_call_id_reuse: AtomicU64,
    pub pending_expired: AtomicU64,
    pub inbox_drop: AtomicU64,
    pub delivered: AtomicU64,
    /// Initial INVITEs whose token matched a claim-mode call but no PENDING
    /// claim accepted them — a scenario/SUT mismatch on a KNOWN call, counted
    /// apart from true orphans (which never correlated at all).
    pub unclaimed: AtomicU64,
    /// Token-slot registrations (draws) rejected because a CONCURRENT call
    /// already owns the token (a number shared under To-user or From-user
    /// correlation). A from-user call makes up to `KEY_DRAWS` draws, so this
    /// counts refused draws, not refused calls.
    pub token_collision: AtomicU64,
    /// Caller INVITEs refused before the wire because, under from-user
    /// correlation, their From URI user is not the call's registered key.
    pub caller_key_mismatch: AtomicU64,
    /// From-user draws refused because the key's previous call ended not ok
    /// within [`RELEASE_HOLD`](super::RELEASE_HOLD). A call makes up to
    /// `KEY_DRAWS` draws, so this counts refused draws, not refused calls.
    pub key_cooling: AtomicU64,
    /// Claims released (call teardown or pending-reap) without ever firing —
    /// an expected inbound leg the SUT never dialed.
    pub claim_unfired: AtomicU64,
    /// Datagrams the per-call loss model deliberately discarded, split by
    /// direction: `out` = never hit the wire (dropped in `send_to`); `in` =
    /// demuxed to the call but discarded before the app read it.
    pub dropped_out: AtomicU64,
    pub dropped_in: AtomicU64,
    sample_cap: usize,
    samples: Mutex<Vec<String>>,
    /// Per-`(reason, CSeq-method)` orphan breakdown, so an orphan burst is
    /// triageable from `/metrics` alone ("stray BYE: N, stray OPTIONS: M")
    /// without a packet capture. Off the hot path (orphans only), so a
    /// `Mutex<map>` is fine.
    /// Orphans by reason and CSeq method, under the family's cap (the method
    /// token is wire-controlled).
    orphan_by_method: OrphanRows,
}

impl MuxStats {
    pub(super) fn new(sample_cap: usize) -> Self {
        Self { sample_cap, ..Default::default() }
    }

    pub(super) fn orphan(&self, reason: OrphanReason, raw: &[u8]) {
        match reason {
            OrphanReason::NoHeader => self.orphan_no_header.fetch_add(1, Ordering::Relaxed),
            OrphanReason::UnknownToken | OrphanReason::NoRoute => {
                self.orphan_unknown_token.fetch_add(1, Ordering::Relaxed)
            }
            OrphanReason::Stray => self.orphan_stray.fetch_add(1, Ordering::Relaxed),
            OrphanReason::Released => self.orphan_released.fetch_add(1, Ordering::Relaxed),
            OrphanReason::Early => self.orphan_early.fetch_add(1, Ordering::Relaxed),
            OrphanReason::CallIdReuse => self.orphan_call_id_reuse.fetch_add(1, Ordering::Relaxed),
        };
        let method = cseq_method_token(raw).unwrap_or_else(|| "none".to_string());
        self.orphan_by_method.0.add(&[reason.label(), &method], 1);
        let mut g = self.samples.lock().unwrap();
        if g.len() < self.sample_cap {
            // Lead the sample with the CSeq (method + number) so a sampled orphan is
            // self-describing for troubleshooting, then the request/response line.
            g.push(format!("[{}] {} | {}", reason.label(), cseq_value(raw), first_line(raw)));
        }
    }

    /// An initial INVITE on a KNOWN (claim-mode) call that no pending claim
    /// accepted: counted apart from orphans, sampled on the same surface.
    pub(super) fn unclaimed(&self, raw: &[u8]) {
        self.unclaimed.fetch_add(1, Ordering::Relaxed);
        let mut g = self.samples.lock().unwrap();
        if g.len() < self.sample_cap {
            g.push(format!("[unclaimed] {} | {}", cseq_value(raw), first_line(raw)));
        }
    }

    /// Every orphan, whatever its reason.
    pub fn orphans_total(&self) -> u64 {
        [
            &self.orphan_no_header,
            &self.orphan_unknown_token,
            &self.orphan_stray,
            &self.orphan_released,
            &self.orphan_early,
            &self.orphan_call_id_reuse,
        ]
        .iter()
        .map(|c| c.load(Ordering::Relaxed))
        .sum()
    }

    /// Bounded orphan samples (the "notify" surface).
    pub fn samples(&self) -> Vec<String> {
        self.samples.lock().unwrap().clone()
    }
}

#[derive(Clone, Copy)]
pub(crate) enum OrphanReason {
    /// An initial INVITE we cannot correlate (no token) — the concerning case.
    NoHeader,
    /// A token present but matching no pending call.
    UnknownToken,
    /// A token matched a call, but its scenario-owned picker chose a label no
    /// registered receiver carries (a scenario routing bug).
    NoRoute,
    /// An unknown Call-ID that is not an initial INVITE (a late straggler).
    Stray,
    /// An INVITE on a Call-ID a finished call released (see `orphan_released`).
    Released,
    /// A leg before its call's caller sent its INVITE (see `orphan_early`).
    Early,
    /// A new INVITE on a caller-owned Call-ID (see `orphan_call_id_reuse`).
    CallIdReuse,
}

impl OrphanReason {
    /// Every reason, in declaration order.
    pub(crate) const ALL: [OrphanReason; 7] = [
        OrphanReason::NoHeader,
        OrphanReason::UnknownToken,
        OrphanReason::NoRoute,
        OrphanReason::Stray,
        OrphanReason::Released,
        OrphanReason::Early,
        OrphanReason::CallIdReuse,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            OrphanReason::NoHeader => "no_header",
            OrphanReason::UnknownToken => "unknown_token",
            OrphanReason::NoRoute => "no_route",
            OrphanReason::Stray => "stray",
            OrphanReason::Released => "released",
            OrphanReason::Early => "early",
            OrphanReason::CallIdReuse => "call_id_reuse",
        }
    }
}

impl MuxCore {
    /// Render the mux Prometheus series.
    pub fn render_prometheus(&self) -> String {
        use crate::catalogue as c;
        let s = self.stats();
        let mut out = String::new();
        let load = |a: &AtomicU64| a.load(Ordering::Relaxed);
        // Orphans are labelled by reason AND CSeq method (`sum by(reason)` still
        // aggregates to the per-reason total for existing queries).
        s.orphan_by_method.0.render(&mut out);
        c::MUX_REGISTRY_SIZE.render_value(&mut out, self.registry_size());
        c::MUX_PENDING_EXPIRED.render_value(&mut out, load(&s.pending_expired));
        c::MUX_UNCLAIMED.render_value(&mut out, load(&s.unclaimed));
        c::MUX_TOKEN_COLLISION.render_value(&mut out, load(&s.token_collision));
        c::MUX_CALLER_KEY_MISMATCH.render_value(&mut out, load(&s.caller_key_mismatch));
        c::MUX_KEY_COOLING.render_value(&mut out, load(&s.key_cooling));
        c::MUX_CLAIM_UNFIRED.render_value(&mut out, load(&s.claim_unfired));
        c::MUX_INBOX_DROP.render_value(&mut out, load(&s.inbox_drop));
        c::MUX_DELIVERED.render_value(&mut out, load(&s.delivered));
        let drops = [load(&s.dropped_out), load(&s.dropped_in)];
        c::DROP.render(&mut out, |series| drops[series.at(0)]);
        out
    }
}
