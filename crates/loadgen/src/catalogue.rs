//! The load generator's catalogued metric families: completed calls and
//! their latency per scenario, the SIP round trips per scenario and exchange,
//! the demux's canaries, the simulated loss, the
//! chaos markers, the offered rate and the process's RSS. [`CATALOGUE`] lists
//! them in `/metrics` order.

use metric_catalogue::{Catalogue, Dim, Family, Labels};
use sip_message::method::Method;

use crate::mux::{Exchange, OrphanReason};

/// A scenario of the run's mix: none declared here; the reporter writes the
/// mix's scenarios from the start.
pub const SCENARIO: Dim = Dim::new("scenario", &[]);

/// A call's result class; a wrong status is `status_<code>`, under its own
/// series; the shape's expected reject is `expected_reject`.
pub const CLASS: Dim = Dim::new(
    "class",
    &[
        "ok",
        "expected_reject",
        "timeout",
        "wrong_method",
        "unexpected",
        "transport",
        "unparseable",
        "rfc_audit_fail",
        "check_fail",
        "rejected",
        "panic",
    ],
);

/// Whether a chaos marker fell within the call's lifetime.
pub const CHAOS: Dim = Dim::new("chaos", &["clear", "near"]);

/// A scenario's named checkpoint: each one observed gets its own series.
pub const CHECKPOINT: Dim = Dim::new("checkpoint", &[]);

const EXCHANGE_VALUES: [&str; 15] = metric_catalogue::label_values!(Exchange::ALL, Exchange::label);
/// A timed SIP exchange, indexed like `Exchange::ALL`; each one observed for
/// a scenario gets its own series.
pub const EXCHANGE: Dim = Dim::new("exchange", &EXCHANGE_VALUES);

const ORPHAN_REASON_VALUES: [&str; 7] =
    metric_catalogue::label_values!(OrphanReason::ALL, OrphanReason::label);
/// Why a datagram matched no call, indexed like `OrphanReason::ALL`.
pub const ORPHAN_REASON: Dim = Dim::new("reason", &ORPHAN_REASON_VALUES);

const ORPHAN_METHOD_VALUES: [&str; 15] = {
    let mut out = ["none"; 15];
    let mut i = 0;
    while i < Method::NATIVE_TOKENS.len() {
        out[i] = Method::NATIVE_TOKENS[i];
        i += 1;
    }
    out
};
/// An orphan's CSeq method: every native method, then `none` for a datagram
/// without one; an extension method gets its own series, under the cap.
pub const ORPHAN_METHOD: Dim = Dim::new("method", &ORPHAN_METHOD_VALUES);

pub const CALLS: Family = Family::counter(
    "loadgen_calls_total",
    Labels::Product(&[SCENARIO, CLASS, CHAOS]),
    "Completed load calls by scenario, result class, and chaos proximity.",
)
.semi_open();

pub const SHED: Family = Family::counter(
    "loadgen_shed_total",
    Labels::Product(&[SCENARIO]),
    "Calls dropped at the max-in-flight cap.",
)
.semi_open();

pub const INFLIGHT: Family =
    Family::gauge("loadgen_inflight", Labels::None, "Calls currently in flight.");

pub const STARTED: Family =
    Family::counter("loadgen_started_total", Labels::None, "Calls started.");

pub const RINGING_EXPECTED: Family = Family::counter(
    "loadgen_ringing_expected_total",
    Labels::None,
    "Calls that reached the ring→answer step (18x denominator).",
);

pub const RINGING_RECEIVED: Family = Family::counter(
    "loadgen_ringing_received_total",
    Labels::None,
    "Of those, calls whose caller received the 18x ringing provisional.",
);

pub const E2E_LATENCY_SECONDS: Family = Family::histogram(
    "loadgen_e2e_latency_seconds",
    Labels::Product(&[SCENARIO]),
    "End-to-end call latency.",
)
.semi_open();

pub const CHECKPOINT_LATENCY_SECONDS: Family = Family::histogram(
    "loadgen_checkpoint_latency_seconds",
    Labels::Product(&[SCENARIO, CHECKPOINT]),
    "Named-checkpoint latency.",
)
.semi_open();

pub const RTT_SECONDS: Family = Family::histogram(
    "loadgen_rtt_seconds",
    Labels::Product(&[SCENARIO, EXCHANGE]),
    "SIP round trip of one exchange, from the first transmission (no scripted timer inside).",
)
.semi_open();

pub const MUX_ORPHAN: Family = Family::counter(
    "loadgen_mux_orphan_total",
    Labels::Product(&[ORPHAN_REASON, ORPHAN_METHOD]),
    "Inbound datagrams that matched no call, by reason and CSeq method.",
)
.capped(&MUX_ORPHAN_OVERFLOW, &["method"]);

pub const MUX_REGISTRY_SIZE: Family =
    Family::gauge("loadgen_mux_registry_size", Labels::None, "Live demux entries (leak canary).");

pub const MUX_PENDING_EXPIRED: Family = Family::counter(
    "loadgen_mux_pending_expired_total",
    Labels::None,
    "Pending callee legs reaped (never arrived).",
);

pub const MUX_UNCLAIMED: Family = Family::counter(
    "loadgen_mux_unclaimed_total",
    Labels::None,
    "Initial INVITEs on a known call that no pending claim accepted.",
);

pub const MUX_TOKEN_COLLISION: Family = Family::counter(
    "loadgen_mux_token_collision_total",
    Labels::None,
    "Token-slot registration draws refused: token already owned by a concurrent call (up to KEY_DRAWS per call).",
);

pub const MUX_CALLER_KEY_MISMATCH: Family = Family::counter(
    "loadgen_mux_caller_key_mismatch_total",
    Labels::None,
    "Caller INVITEs refused before the wire: From user is not the call key (from-user correlation).",
);

pub const MUX_KEY_COOLING: Family = Family::counter(
    "loadgen_mux_key_cooling_total",
    Labels::None,
    "From-user draws refused: the key's previous call ended not ok within 64*T1 (up to KEY_DRAWS per call).",
);

pub const MUX_CLAIM_UNFIRED: Family = Family::counter(
    "loadgen_mux_claim_unfired_total",
    Labels::None,
    "Claims released without firing (expected inbound leg never came).",
);

pub const MUX_INBOX_DROP: Family = Family::counter(
    "loadgen_mux_inbox_drop_total",
    Labels::None,
    "Datagrams dropped on a full call inbox.",
);

pub const MUX_DELIVERED: Family =
    Family::counter("loadgen_mux_delivered_total", Labels::None, "Datagrams demuxed to a call.");

pub const DROP: Family = Family::counter(
    "loadgen_drop_total",
    Labels::Product(&[Dim::new("dir", &["out", "in"])]),
    "Datagrams dropped by the simulated packet-loss model, by direction.",
);

pub const CHAOS_MARKERS: Family = Family::counter(
    "loadgen_chaos_markers_total",
    Labels::None,
    "Chaos markers recorded by the loadgen.",
);

pub const CHAOS_MARKERS_RETAINED: Family = Family::gauge(
    "loadgen_chaos_markers_retained",
    Labels::Product(&[Dim::new("kind", &[])]),
    "Chaos markers currently retained, by kind.",
)
.semi_open();

pub const TARGET_CPS: Family = Family::gauge(
    "loadgen_target_cps",
    Labels::None,
    "Current offered call-rate target (calls/s; 0 = paused).",
);

pub const PROCESS_RESIDENT_MEMORY: Family = Family::gauge(
    "loadgen_process_resident_memory_bytes",
    Labels::None,
    "Load generator RSS; NaN where /proc does not say.",
);

pub const MUX_ORPHAN_OVERFLOW: Family = Family::counter(
    "loadgen_mux_orphan_overflow_total",
    Labels::None,
    "observations of loadgen_mux_orphan_total past its cap, each counted on its series whose method reads _overflow",
);

/// The load generator's `/metrics` catalogue.
pub const CATALOGUE: Catalogue = Catalogue {
    binary: "loadgen",
    sections: &[&[
        CALLS,
        SHED,
        INFLIGHT,
        STARTED,
        RINGING_EXPECTED,
        RINGING_RECEIVED,
        E2E_LATENCY_SECONDS,
        CHECKPOINT_LATENCY_SECONDS,
        RTT_SECONDS,
        MUX_ORPHAN,
        MUX_ORPHAN_OVERFLOW,
        MUX_REGISTRY_SIZE,
        MUX_PENDING_EXPIRED,
        MUX_UNCLAIMED,
        MUX_TOKEN_COLLISION,
        MUX_CALLER_KEY_MISMATCH,
        MUX_KEY_COOLING,
        MUX_CLAIM_UNFIRED,
        MUX_INBOX_DROP,
        MUX_DELIVERED,
        DROP,
        CHAOS_MARKERS,
        CHAOS_MARKERS_RETAINED,
        TARGET_CPS,
        PROCESS_RESIDENT_MEMORY,
    ]],
};

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::chaos::ChaosLog;
    use crate::mux::{Correlation, EndpointSpec, MuxCore, Role};
    use crate::rate::RateHandle;
    use crate::report::{Reporter, ReporterCfg};

    /// The body holds its catalogue exactly: before any call, the run's
    /// scenarios are already written; a measured round trip is written under
    /// its scenario and exchange.
    #[tokio::test]
    #[ignore = "slow lane: loadgen"]
    async fn the_body_holds_its_catalogue_exactly() {
        let sim = Arc::new(sip_net::SimulatedSignalingNetwork::new(1));
        let core = MuxCore::bind_on(
            sim.as_ref(),
            vec![EndpointSpec { addr: "127.0.0.1:7001".parse().unwrap(), role: Role::Caller }],
            Correlation::header("X-Loadgen-Id"),
            64,
            8,
            Duration::from_secs(20),
            sip_clock::Clock::test_at(0),
        )
        .await
        .unwrap();
        let reporter = Reporter::new(ReporterCfg { sample_cap: 0, background_record_every: 0 });
        reporter.declare_scenarios(["basic_call"]);
        reporter.record_rtts("basic_call", &[(Exchange::Invite100, Duration::from_millis(2))]);
        let chaos = ChaosLog::new(sip_clock::Clock::test_at(0));
        let text = crate::app::metrics_body(&reporter, &core, &chaos, &RateHandle::new(5.0));
        if let Err(mismatches) = CATALOGUE.check(&text) {
            panic!("{mismatches:#?}\n{text}");
        }
        assert!(text.contains(
            "loadgen_calls_total{scenario=\"basic_call\",class=\"ok\",chaos=\"clear\"} 0\n"
        ));
        assert!(
            text.contains("loadgen_e2e_latency_seconds_count{scenario=\"basic_call\"} 0\n"),
            "a declared scenario's call latency is written from the start: {text}"
        );
        assert!(!text.contains("loadgen_e2e_seconds"), "no lifetime quantile gauge: {text}");
        assert!(text.contains(
            "loadgen_rtt_seconds_count{scenario=\"basic_call\",exchange=\"invite_100\"} 1\n"
        ));
    }
}
