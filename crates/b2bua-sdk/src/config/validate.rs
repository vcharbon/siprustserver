//! The check of a worker's tunables: every value that would break a call is
//! reported, each message naming the runner variables involved.

use super::B2buaConfig;

impl B2buaConfig {
    /// Validate the tunables of the **final** configuration (the runner kit
    /// calls this once, after the runner's composition, and refuses to start on
    /// `Err`; unit/sim harnesses construct configs directly and skip it).
    /// `Err` lists every violation, `; `-separated, each naming the runner
    /// variables (`B2BUA_*`, `LIMITER_*`) it involves, or the field when no
    /// variable states it.
    pub fn validate(&self) -> Result<(), String> {
        let mut out = Vec::new();
        self.check(&mut out);
        if out.is_empty() {
            Ok(())
        } else {
            Err(out.join("; "))
        }
    }

    fn check(&self, out: &mut Vec<String>) {
        if let Some((host, port)) = self.front.outbound_proxy() {
            if host.trim_matches(['[', ']']).parse::<std::net::IpAddr>().is_err() {
                out.push(format!(
                    "B2BUA_OUTBOUND_PROXY={host}:{port} is no IP literal: the transaction layer \
                     sends what it builds itself there and resolves no name"
                ));
            }
        }
        if let Some(r) = self.glare_retry_after {
            if r.min_sec > r.max_sec {
                out.push(format!(
                    "glare_retry_after={}..={}: an empty range picks no Retry-After",
                    r.min_sec, r.max_sec
                ));
            }
        }
        if !(1..=100).contains(&self.new_call_permit_share_percent) {
            out.push(format!(
                "B2BUA_NEW_CALL_PERMIT_SHARE_PCT={} outside 1..=100: new calls need at least \
                 one handler permit and can hold no more than the whole pool",
                self.new_call_permit_share_percent
            ));
        }
        if self.new_call_queue_headroom_percent > 99 {
            out.push(format!(
                "B2BUA_NEW_CALL_QUEUE_HEADROOM_PCT={} above 99: a headroom of the whole cap \
                 refuses every normal new call",
                self.new_call_queue_headroom_percent
            ));
        }
        if self.limiter_refresh_sec <= 0 {
            out.push(format!(
                "LIMITER_REFRESH_SECONDS={}: a counted call refreshes at a positive period",
                self.limiter_refresh_sec
            ));
        }
        let refresh_ms = self.limiter_refresh_sec.saturating_mul(1000);
        let batch = i64::try_from(self.limiter_refresh_batch_ms);
        if !batch.is_ok_and(|batch| (1..refresh_ms).contains(&batch)) {
            out.push(format!(
                "LIMITER_REFRESH_BATCH_MS={} not in 1..{refresh_ms} (LIMITER_REFRESH_SECONDS): \
                 a refresh falling due must leave within one refresh period",
                self.limiter_refresh_batch_ms
            ));
        }
        if self.limiter_refresh_batch_max == 0 {
            out.push(
                "LIMITER_REFRESH_BATCH_MAX=0: a refresh request carries at least one key"
                    .to_string(),
            );
        }
        if self.limiter_release_queue_cap == 0 {
            out.push(
                "LIMITER_RELEASE_QUEUE_CAP=0: the release queue needs room for one release"
                    .to_string(),
            );
        }
        if self.max_messages_per_call_lifetime == 0 {
            out.push(
                "B2BUA_MAX_MESSAGES_PER_CALL_LIFETIME=0: the per-call work bound has no off; \
                 set it far above any healthy call"
                    .to_string(),
            );
        }
        // The floor also keeps the interval above the probe's own Timer F
        // (32 s), so a probe's deadline passes before the next probe leaves.
        if self.keepalive_interval_sec < Self::MIN_KEEPALIVE_SEC {
            out.push(format!(
                "B2BUA_KEEPALIVE_SEC={} < min {} s (2 min): a shorter in-dialog OPTIONS cadence \
                 breaks long-hold traffic",
                self.keepalive_interval_sec,
                Self::MIN_KEEPALIVE_SEC
            ));
        }
        // The reaper's staleness verdict is only provable when a healthy call is
        // guaranteed at least one liveness-bearing event (its keepalive OPTIONS
        // `200`) inside the idle window (ADR-0020 X4).
        if self.reaper_idle_max_sec != 0
            && self.reaper_idle_max_sec < 2 * self.keepalive_interval_sec
        {
            out.push(format!(
                "reaper_idle_max_sec={} < 2 × B2BUA_KEEPALIVE_SEC={}: a healthy call is only \
                 provably dead after missing at least two keepalive cycles (0 derives 3× \
                 automatically)",
                self.reaper_idle_max_sec, self.keepalive_interval_sec
            ));
        }
        // ELU is clamped to [0, 1]: a threshold <= 0 (or NaN) trips for every
        // call; >= 1.0 is the documented "disable".
        if !(self.overload_panic_elu_threshold.is_finite()
            && self.overload_panic_elu_threshold > 0.0)
        {
            out.push(format!(
                "B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD={} is not a positive fraction: ELU is \
                 clamped to [0,1], so a value <= 0 (or NaN) would 503 every new INVITE the \
                 instant load rises. Use a value in (0, 1] (>= 1.0 disables the backstop)",
                self.overload_panic_elu_threshold
            ));
        }
        // Above the 32 s in-dialog Timer B (the out-of-dialog window stays the
        // long one), at most the telephony ceiling.
        if !(Self::MIN_INVITE_TXN_TIMEOUT_SEC..=Self::MAX_INVITE_TXN_TIMEOUT_SEC)
            .contains(&self.invite_txn_timeout_sec)
        {
            out.push(format!(
                "B2BUA_INVITE_TXN_TIMEOUT_SEC={} outside the supported {}..={} s range",
                self.invite_txn_timeout_sec,
                Self::MIN_INVITE_TXN_TIMEOUT_SEC,
                Self::MAX_INVITE_TXN_TIMEOUT_SEC
            ));
        }
        // Below the two-rung floor the hop has heard the INVITE once; above
        // Timer B the transaction reports the hop dead on its own.
        if !(Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC
            ..=Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC)
            .contains(&self.invite_first_response_timeout_sec)
        {
            out.push(format!(
                "B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC={} outside the supported {}..={} s \
                 range",
                self.invite_first_response_timeout_sec,
                Self::MIN_INVITE_FIRST_RESPONSE_TIMEOUT_SEC,
                Self::MAX_INVITE_FIRST_RESPONSE_TIMEOUT_SEC
            ));
        }
        // Above the transaction bound the ordering inverts: the txn layer CANCELs
        // the b-leg on its own, the app give-up authors a SECOND final, and the
        // callee's 487 lands on a dead client txn.
        if self.setup_timeout_ms > 0
            && self.setup_timeout_ms >= self.invite_txn_timeout_sec.saturating_mul(1000)
        {
            out.push(format!(
                "B2BUA_SETUP_TIMEOUT_MS={} is not below B2BUA_INVITE_TXN_TIMEOUT_SEC={} s: the \
                 app setup deadline must fire strictly before the transaction bound, or the txn \
                 layer tears the b-leg down first and the caller takes two finals on one INVITE",
                self.setup_timeout_ms, self.invite_txn_timeout_sec
            ));
        }
        // A ringing call stamps no ledger liveness until its setup resolves.
        if self.reaper_idle_max_sec != 0 && self.reaper_idle_max_sec <= self.invite_txn_timeout_sec
        {
            out.push(format!(
                "reaper_idle_max_sec={} <= B2BUA_INVITE_TXN_TIMEOUT_SEC={}: the reaper idle \
                 window must outlast the longest configured ring, or a legitimately \
                 still-ringing call is reaped before its own setup deadline",
                self.reaper_idle_max_sec, self.invite_txn_timeout_sec
            ));
        }
        // The un-ACKed 2xx give-up is a deadline, never a switch (RFC 3261
        // §13.3.1.4).
        if self.ack_timeout_sec <= 0 {
            out.push(format!(
                "B2BUA_ACK_TIMEOUT_SEC={} is not a positive deadline: an un-ACKed 2xx always \
                 ends the session (RFC 3261 §13.3.1.4), the knob only says how many seconds \
                 to wait first (default {})",
                self.ack_timeout_sec,
                Self::DEFAULT_ACK_TIMEOUT_SEC
            ));
        }
        // An enabled bucket (size > 0) with no refill drains once and 503s
        // every new INVITE after the first burst.
        if self.cps_bucket_size > 0 && self.cps_bucket_rate == 0 {
            out.push(format!(
                "B2BUA_CPS_BUCKET_RATE=0 with B2BUA_CPS_BUCKET_SIZE={} (CPS gate enabled): the \
                 token bucket would drain once and never refill, 503-ing every new INVITE after \
                 the first burst. Set a positive refill rate, or set B2BUA_CPS_BUCKET_SIZE=0 to \
                 disable the hard CPS gate",
                self.cps_bucket_size
            ));
        }
        if let Err(e) = self.capacity.validate() {
            out.push(e);
        }
        if let Err(v) = self.event_queue_gate.validate() {
            out.push(match v {
                super::GateViolation::Zero(b) => format!("{}=0: must be positive", b.env()),
                super::GateViolation::NotBelow { low, high, why } => {
                    format!("{} must be below {}: {why}", low.env(), high.env())
                }
                super::GateViolation::PastTheQueue => format!(
                    "B2BUA_EVENT_QUEUE_REFUSE_DEPTH={} exceeds the event queue's bound {}",
                    self.event_queue_gate.refuse_depth,
                    super::EVENT_QUEUE_CAPACITY
                ),
            });
        }
    }
}
