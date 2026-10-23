//! The boot check of a worker's tunables: the first value that would break a
//! call or the replicas refuses the start.

use super::B2buaConfig;

impl B2buaConfig {
    /// Validate operator-supplied tunables at **boot** (the runner calls this and
    /// refuses to start on `Err`; unit/sim harnesses construct configs directly
    /// and skip it). Returns the first violation as a human-readable message.
    pub fn validate(&self) -> Result<(), String> {
        if let Some(r) = self.glare_retry_after {
            if r.min_sec > r.max_sec {
                return Err(format!(
                    "glare_retry_after={}..={}: an empty range picks no Retry-After",
                    r.min_sec, r.max_sec
                ));
            }
        }
        if !(1..=100).contains(&self.new_call_permit_share_percent) {
            return Err(format!(
                "new_call_permit_share_percent={} outside 1..=100: new calls need at least \
                 one handler permit and can hold no more than the whole pool",
                self.new_call_permit_share_percent
            ));
        }
        if self.new_call_queue_headroom_percent > 99 {
            return Err(format!(
                "new_call_queue_headroom_percent={} above 99: a headroom of the whole cap \
                 refuses every normal new call",
                self.new_call_queue_headroom_percent
            ));
        }
        if self.limiter_refresh_sec <= 0 {
            return Err(format!(
                "limiter_refresh_sec={}: a counted call refreshes at a positive period",
                self.limiter_refresh_sec
            ));
        }
        let refresh_ms = self.limiter_refresh_sec.saturating_mul(1000);
        let batch = i64::try_from(self.limiter_refresh_batch_ms);
        if !batch.is_ok_and(|batch| (1..refresh_ms).contains(&batch)) {
            return Err(format!(
                "limiter_refresh_batch_ms={} not in 1..{refresh_ms} (limiter_refresh_sec): a \
                 refresh falling due must leave within one refresh period",
                self.limiter_refresh_batch_ms
            ));
        }
        if self.limiter_refresh_batch_max == 0 {
            return Err("limiter_refresh_batch_max=0: a refresh request carries at least one \
                        key"
            .to_string());
        }
        if self.limiter_release_queue_cap == 0 {
            return Err("limiter_release_queue_cap=0: the release queue needs room for \
                        one release"
                .to_string());
        }
        if self.reaper_enabled && self.max_messages_per_call_lifetime == 0 {
            return Err("max_messages_per_call_lifetime=0: the per-call work bound has no \
                        off; set it far above any healthy call"
                .to_string());
        }
        if self.keepalive_interval_sec < Self::MIN_KEEPALIVE_SEC {
            return Err(format!(
                "keepalive_interval_sec={} < min {} s (2 min): a shorter in-dialog \
                 OPTIONS cadence breaks long-hold traffic",
                self.keepalive_interval_sec,
                Self::MIN_KEEPALIVE_SEC
            ));
        }
        // The reaper's staleness verdict is only provable when a healthy call is
        // guaranteed at least one liveness-bearing event (its keepalive OPTIONS
        // `200` — only real SIP traffic stamps the ledger) inside the idle
        // window — below 2 keepalive intervals a merely-quiet call could be
        // reaped (ADR-0020 X4).
        if self.reaper_idle_max_sec != 0
            && self.reaper_idle_max_sec < 2 * self.keepalive_interval_sec
        {
            return Err(format!(
                "reaper_idle_max_sec={} < 2 × keepalive_interval_sec={}: a healthy \
                 call is only provably dead after missing at least two keepalive \
                 cycles (0 derives 3× automatically)",
                self.reaper_idle_max_sec, self.keepalive_interval_sec
            ));
        }
        // ── Worker overload protection (CPS bucket + panic-ELU rungs) ──
        // The panic-ELU backstop 503s a new INVITE when the worker's own
        // EWMA-smoothed Event-Loop Utilization exceeds this fraction. ELU is
        // clamped to [0, 1], so a threshold <= 0 (or NaN) would trip for *every*
        // call the instant load is non-zero — the worker could never admit an
        // INVITE. (>= 1.0 is the documented "disable" and is allowed: the clamped
        // ELU never exceeds 1.)
        if !(self.overload_panic_elu_threshold.is_finite()
            && self.overload_panic_elu_threshold > 0.0)
        {
            return Err(format!(
                "overload_panic_elu_threshold={} is not a positive fraction: ELU is \
                 clamped to [0,1], so a value <= 0 (or NaN) would 503 every new \
                 INVITE the instant load rises. Use a value in (0, 1] (>= 1.0 \
                 disables the backstop)",
                self.overload_panic_elu_threshold
            ));
        }
        // The transaction bound must sit in the supported range: above the 32 s
        // in-dialog Timer B (the floor keeps the out-of-dialog window the LONG
        // one), at most the telephony ceiling.
        if !(Self::MIN_INVITE_TXN_TIMEOUT_SEC..=Self::MAX_INVITE_TXN_TIMEOUT_SEC)
            .contains(&self.invite_txn_timeout_sec)
        {
            return Err(format!(
                "invite_txn_timeout_sec={} outside the supported {}..={} s range",
                self.invite_txn_timeout_sec,
                Self::MIN_INVITE_TXN_TIMEOUT_SEC,
                Self::MAX_INVITE_TXN_TIMEOUT_SEC
            ));
        }
        // The first-response deadline sits between the two-rung floor and
        // Timer B: below the floor the hop has heard the INVITE once, above
        // Timer B the transaction reports the hop dead on its own.
        if !(Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC
            ..=Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC)
            .contains(&self.invite_first_response_timeout_sec)
        {
            return Err(format!(
                "invite_first_response_timeout_sec={} outside the supported {}..={} s range",
                self.invite_first_response_timeout_sec,
                Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC,
                Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC
            ));
        }
        // Above the transaction bound the ordering inverts: the txn layer CANCELs the b-leg on
        // its own, the app give-up authors a SECOND final, and the callee's 487 lands on a dead
        // client txn (never ACKed). The app setup deadline must therefore fire strictly first.
        if self.setup_timeout_ms > 0 && self.setup_timeout_ms >= self.invite_txn_timeout_sec * 1000
        {
            return Err(format!(
                "setup_timeout_ms={} >= invite_txn_timeout_sec={} s: the app setup \
                 deadline must fire strictly before the transaction bound, or the \
                 txn layer tears the b-leg down first and the caller takes two \
                 finals on one INVITE",
                self.setup_timeout_ms, self.invite_txn_timeout_sec
            ));
        }
        // A ringing call stamps no ledger liveness until its setup resolves, so
        // an explicit reaper idle window at or under the transaction bound would
        // reap a call that is legitimately still ringing (the derived window is
        // floored above the bound automatically — see `reaper_idle_max_ms`).
        if self.reaper_idle_max_sec != 0 && self.reaper_idle_max_sec <= self.invite_txn_timeout_sec
        {
            return Err(format!(
                "reaper_idle_max_sec={} <= invite_txn_timeout_sec={}: the reaper idle \
                 window must outlast the longest configured ring, or a legitimately \
                 still-ringing call is reaped before its own setup deadline",
                self.reaper_idle_max_sec, self.invite_txn_timeout_sec
            ));
        }
        // The un-ACKed 2xx give-up is a deadline, never a switch (RFC 3261
        // §13.3.1.4): a non-positive value would configure a leaking session.
        if self.ack_timeout_sec <= 0 {
            return Err(format!(
                "ack_timeout_sec={} is not a positive deadline: an un-ACKed 2xx \
                 always ends the session (RFC 3261 §13.3.1.4), the knob only says \
                 how many seconds to wait first (default {})",
                self.ack_timeout_sec,
                Self::DEFAULT_ACK_TIMEOUT_SEC
            ));
        }
        // The CPS bucket refills at `cps_bucket_rate` tokens/s up to
        // `cps_bucket_size`. If the gate is enabled (size > 0) but the rate is 0,
        // the bucket drains once and never refills — after the first burst EVERY
        // new INVITE is 503'd forever. (size == 0 disables the hard CPS gate, so
        // the rate is moot.)
        if self.cps_bucket_size > 0 && self.cps_bucket_rate == 0 {
            return Err(format!(
                "cps_bucket_rate=0 with cps_bucket_size={} (CPS gate enabled): the \
                 token bucket would drain once and never refill, 503-ing every new \
                 INVITE after the first burst. Set a positive refill rate, or set \
                 cps_bucket_size=0 to disable the hard CPS gate",
                self.cps_bucket_size
            ));
        }
        self.capacity.validate()?;
        Ok(())
    }
}
