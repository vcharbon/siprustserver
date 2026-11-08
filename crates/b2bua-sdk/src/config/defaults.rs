//! [`B2buaConfig`]'s defaults.

use super::*;

impl Default for B2buaConfig {
    fn default() -> Self {
        Self {
            parser_limits: sip_message::SipParserLimits::DEFAULT,
            self_ordinal: "w0".to_string(),
            cluster_key: None,
            sip_local_ip: "127.0.0.1".to_string(),
            sip_local_port: 5060,
            front: Front::Standalone,
            event_dispatch_concurrency: 1024,
            per_call_queue_depth: 64,
            per_call_queue_cap: 200_000,
            new_call_permit_share_percent: 50,
            new_call_queue_headroom_percent: 5,
            // Loop/runaway guard, per keepalive interval (the tick resets the
            // counter — see the field doc): 200, so a legitimate per-interval
            // burst stays well under it while a flood/glare loop is still capped
            // inside one window. Override with `B2BUA_MAX_MESSAGES_PER_CALL`.
            max_messages_per_call: 200,
            max_messages_per_call_lifetime: 100_000,
            cdr_buffer_queue_max: 1_024,
            refer_subscription_expiry_sec: 60,
            refer_reinvite_answer_sec: 32,
            refer_overall_safety_sec: 120,
            release_reroute_guard_sec: 120,
            keepalive_interval_sec: 300,
            limiter_refresh_sec: 40,
            limiter_release_queue_cap: 100_000,
            limiter_refresh_batch_ms: 1_000,
            limiter_refresh_batch_max: 1_000,
            reaper_enabled: true,
            model_invariants: cfg!(debug_assertions),
            reaper_sweep_interval_sec: 30,
            reaper_idle_max_sec: 0,
            setup_timeout_ms: 150_000,
            invite_txn_timeout_sec: Self::DEFAULT_INVITE_TXN_TIMEOUT_SEC,
            invite_first_response_timeout_sec: Self::DEFAULT_INVITE_FIRST_RESPONSE_TIMEOUT_SEC,
            ack_timeout_sec: Self::DEFAULT_ACK_TIMEOUT_SEC,
            // CPS bucket and panic-ELU rungs. Defaults match the env knobs
            // (B2BUA_CPS_BUCKET_SIZE / B2BUA_CPS_BUCKET_RATE /
            // B2BUA_OVERLOAD_PANIC_ELU_THRESHOLD / B2BUA_RETRY_AFTER_BASE_SEC).
            cps_bucket_size: 1000,
            cps_bucket_rate: 500,
            overload_panic_elu_threshold: 0.75,
            retry_after_base_sec: 5,
            retry_after_jitter_sec: 5,
            glare_retry_after: None,
            // b-leg admission allow-list. Default is the single K8s in-cluster
            // DNS suffix `.svc.cluster.local`; production traffic (pod FQDNs)
            // always passes, bogus hostnames are 503'd pre-leg. The paused-clock
            // test harness builds configs directly; a fixture that routes to a
            // non-suffixed host (e.g. `bob` / a loopback name) must set `["*"]`
            // or add its suffix to opt out of the gate.
            worker_allowed_target_suffixes: vec![".svc.cluster.local".to_string()],
            // Decision-backend deadline: one knob for all three methods (ADR-0022).
            call_control_timeout_ms: Self::DEFAULT_CALL_CONTROL_TIMEOUT_MS,
            relay_headers: Vec::new(),
            privacy_service: true,
            relay_policy: sip_message::generators::RelayPolicy::transparent(),
            relayed_cancel_reason: crate::release_reason::CancelReason::Verbatim,
            own_release_reason: None,
            default_sdp: None,
            sdp_form: std::sync::Arc::new(AsWritten),
            node_capabilities: sip_message::generators::CapabilitySet::default(),
            minted_final_advertisement: sip_message::generators::CapabilitySet::silent(),
            cdr: CdrConfig::default(),
            capacity: CapacityConfig::default(),
            event_queue_gate: EventQueueGateConfig::default(),
            service_params: ServiceParams::default(),
            caps: CallCaps::DEFAULT,
        }
    }
}
