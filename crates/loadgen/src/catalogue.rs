//! The load generator's catalogued metric families: completed calls and
//! their latency per scenario, the accepted branch or failed step of each,
//! the SIP round trips per scenario and exchange,
//! the demux's canaries, the simulated loss, the
//! chaos markers, the offered rate and the process's RSS. [`CATALOGUE`] lists
//! them in `/metrics` order.

use metric_catalogue::{Catalogue, Dim, Family, Labels};
use sip_message::method::Method;

use crate::mux::{Exchange, OrphanReason};

/// The load stream a generator process runs (`--stream`): every family's
/// first label.
pub const STREAM: Dim = Dim::new("stream", &[]);
/// The stream a series names before the run names one.
pub const DEFAULT_STREAM: &str = "main";
/// The treatment catalog shape id of a call.
pub const SHAPE: Dim = Dim::new("shape", &[]);

/// A call's result class; a wrong status is `status_<code>`, under its own
/// series. Every accepted branch is `ok`.
pub const CLASS: Dim = Dim::new(
    "class",
    &[
        "ok",
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

/// The accepted ending a call ended on: a scenario's declared branch name;
/// each one observed gets its own series.
pub const BRANCH: Dim = Dim::new("branch", &[]);

/// The step a call failed at (`<who>[:<kind>#<index>]@<barrier>`); each one
/// observed gets its own series.
pub const STEP: Dim = Dim::new("step", &[]);

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
    Labels::Product(&[STREAM, SHAPE, CLASS, CHAOS]),
    "Completed load calls by shape, result class, and chaos proximity.",
)
.semi_open();

pub const BRANCHES: Family = Family::counter(
    "loadgen_branch_total",
    Labels::Product(&[STREAM, SHAPE, BRANCH]),
    "Calls classed ok, by shape and the accepted branch they ended on.",
)
.semi_open();

pub const FAILED_STEPS: Family = Family::counter(
    "loadgen_failed_step_total",
    Labels::Product(&[STREAM, SHAPE, STEP]),
    "Calls whose scenario failed, by shape and the step they failed at.",
)
.semi_open();

pub const SHED: Family = Family::counter(
    "loadgen_shed_total",
    Labels::Product(&[STREAM, SHAPE]),
    "Calls dropped at the max-in-flight cap.",
)
.semi_open();

pub const INFLIGHT: Family =
    Family::gauge("loadgen_inflight", Labels::Product(&[STREAM]), "Calls currently in flight.")
        .semi_open();

pub const STARTED: Family =
    Family::counter("loadgen_started_total", Labels::Product(&[STREAM]), "Calls started.")
        .semi_open();

pub const RINGING_EXPECTED: Family = Family::counter(
    "loadgen_ringing_expected_total",
    Labels::Product(&[STREAM]),
    "Calls that reached the ring→answer step (18x denominator).",
)
.semi_open();

pub const RINGING_RECEIVED: Family = Family::counter(
    "loadgen_ringing_received_total",
    Labels::Product(&[STREAM]),
    "Of those, calls whose caller received the 18x ringing provisional.",
)
.semi_open();

pub const E2E_LATENCY_SECONDS: Family = Family::histogram(
    "loadgen_e2e_latency_seconds",
    Labels::Product(&[STREAM, SHAPE]),
    "End-to-end call latency.",
)
.semi_open();

pub const CHECKPOINT_LATENCY_SECONDS: Family = Family::histogram(
    "loadgen_checkpoint_latency_seconds",
    Labels::Product(&[STREAM, SHAPE, CHECKPOINT]),
    "Named-checkpoint latency.",
)
.semi_open();

pub const RTT_SECONDS: Family = Family::histogram(
    "loadgen_rtt_seconds",
    Labels::Product(&[STREAM, SHAPE, EXCHANGE]),
    "SIP round trip of one exchange, from the first transmission (no scripted timer inside).",
)
.semi_open();

pub const MUX_ORPHAN: Family = Family::counter(
    "loadgen_mux_orphan_total",
    Labels::Product(&[STREAM, ORPHAN_REASON, ORPHAN_METHOD]),
    "Inbound datagrams that matched no call, by reason and CSeq method.",
)
.capped(&MUX_ORPHAN_OVERFLOW, &["method"]);

pub const MUX_REGISTRY_SIZE: Family = Family::gauge(
    "loadgen_mux_registry_size",
    Labels::Product(&[STREAM]),
    "Live demux entries (leak canary).",
)
.semi_open();

pub const MUX_PENDING_EXPIRED: Family = Family::counter(
    "loadgen_mux_pending_expired_total",
    Labels::Product(&[STREAM]),
    "Pending callee legs reaped (never arrived).",
)
.semi_open();

pub const MUX_UNCLAIMED: Family = Family::counter(
    "loadgen_mux_unclaimed_total",
    Labels::Product(&[STREAM]),
    "Initial INVITEs on a known call that no pending claim accepted.",
)
.semi_open();

pub const MUX_TOKEN_COLLISION: Family = Family::counter(
    "loadgen_mux_token_collision_total",
    Labels::Product(&[STREAM]),
    "Token-slot registration draws refused: token already owned by a concurrent call (up to KEY_DRAWS per call).",
)
.semi_open();

pub const MUX_CALLER_KEY_MISMATCH: Family = Family::counter(
    "loadgen_mux_caller_key_mismatch_total",
    Labels::Product(&[STREAM]),
    "Caller INVITEs refused before the wire: From user is not the call key (from-user correlation).",
)
.semi_open();

pub const MUX_KEY_COOLING: Family = Family::counter(
    "loadgen_mux_key_cooling_total",
    Labels::Product(&[STREAM]),
    "From-user draws refused: the key's previous call ended not ok within 64*T1 (up to KEY_DRAWS per call).",
)
.semi_open();

pub const MUX_CLAIM_UNFIRED: Family = Family::counter(
    "loadgen_mux_claim_unfired_total",
    Labels::Product(&[STREAM]),
    "Claims released without firing (expected inbound leg never came).",
)
.semi_open();

pub const MUX_INBOX_DROP: Family = Family::counter(
    "loadgen_mux_inbox_drop_total",
    Labels::Product(&[STREAM]),
    "Datagrams dropped on a full call inbox.",
)
.semi_open();

pub const MUX_DELIVERED: Family = Family::counter(
    "loadgen_mux_delivered_total",
    Labels::Product(&[STREAM]),
    "Datagrams demuxed to a call.",
)
.semi_open();

pub const DROP: Family = Family::counter(
    "loadgen_drop_total",
    Labels::Product(&[STREAM, Dim::new("dir", &["out", "in"])]),
    "Datagrams dropped by the simulated packet-loss model, by direction.",
)
.semi_open();

pub const CHAOS_MARKERS: Family = Family::counter(
    "loadgen_chaos_markers_total",
    Labels::Product(&[STREAM]),
    "Chaos markers recorded by the loadgen.",
)
.semi_open();

pub const CHAOS_MARKERS_RETAINED: Family = Family::gauge(
    "loadgen_chaos_markers_retained",
    Labels::Product(&[STREAM, Dim::new("kind", &[])]),
    "Chaos markers currently retained, by kind.",
)
.semi_open();

pub const TARGET_CPS: Family = Family::gauge(
    "loadgen_target_cps",
    Labels::Product(&[STREAM]),
    "Current offered call-rate target (calls/s; 0 = paused).",
)
.semi_open();

pub const PROCESS_RESIDENT_MEMORY: Family = Family::gauge(
    "loadgen_process_resident_memory_bytes",
    Labels::Product(&[STREAM]),
    "Load generator RSS; NaN where /proc does not say.",
)
.semi_open();

/// A call-outcome record's `ending`.
pub const ENDING: Dim = Dim::new(
    "ending",
    &["answered", "rejected", "abandoned", "timeout", "cut", "stranded", "failed"],
);
/// Why a call was never placed (its correlation key's refusal).
pub const NOT_PLACED_REASON: Dim = Dim::new("reason", &[]);

pub const OUTCOME_RECORDS: Family = Family::counter(
    "loadgen_outcome_records_total",
    Labels::Product(&[STREAM, SHAPE, ENDING]),
    "Call-outcome records written, one per call placed, by stream, shape and ending.",
)
.semi_open();

pub const NOT_PLACED: Family = Family::counter(
    "loadgen_not_placed_total",
    Labels::Product(&[STREAM, SHAPE, NOT_PLACED_REASON]),
    "Calls refused before any datagram (no record written), by stream, shape and reason.",
)
.semi_open();

pub const CALLS_OFFERED: Family = Family::counter(
    "loadgen_calls_offered_total",
    Labels::Product(&[STREAM, SHAPE]),
    "Calls the rate offered, by stream and shape: placed, not placed or failed by the generator.",
)
.semi_open();

/// Why the generator failed a call it offered.
pub const GENERATOR_FAILURE_REASON: Dim = Dim::new("reason", &[]);

pub const GENERATOR_FAILURES: Family = Family::counter(
    "loadgen_generator_failures_total",
    Labels::Product(&[STREAM, SHAPE, GENERATOR_FAILURE_REASON]),
    "Calls the generator itself failed (no record written), by stream, shape and reason.",
)
.semi_open();

/// Where a record was lost: `writer` (never written anywhere: the writer's
/// queue was full or closed), `push` (in the file, not in the log store).
pub const DROP_STAGE: Dim = Dim::new("stage", &["writer", "push"]);

pub const RECORDS_PUSH_FAILED: Family = Family::counter(
    "loadgen_outcome_records_push_failed_total",
    Labels::Product(&[STREAM]),
    "Pushes of a batch of call-outcome records to the log store that failed (each attempt).",
)
.semi_open();

pub const RECORDS_DROPPED: Family = Family::counter(
    "loadgen_outcome_records_dropped_total",
    Labels::Product(&[STREAM, DROP_STAGE]),
    "Call-outcome records lost, by stream and where: never written, or not in the log store.",
)
.semi_open();

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
        BRANCHES,
        FAILED_STEPS,
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
        OUTCOME_RECORDS,
        NOT_PLACED,
        CALLS_OFFERED,
        GENERATOR_FAILURES,
        RECORDS_PUSH_FAILED,
        RECORDS_DROPPED,
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
        let file = std::env::temp_dir().join(format!("loadgen-cat-{}.jsonl", std::process::id()));
        let outcomes =
            crate::outcome::OutcomeSink::new("r", "loadgen", "main", &file, None).unwrap();
        let rate = RateHandle::new(5.0);
        let text = crate::app::metrics_body(&reporter, &core, &chaos, &rate, &outcomes);
        outcomes.close();
        let _ = std::fs::remove_file(&file);
        if let Err(mismatches) = CATALOGUE.check(&text) {
            panic!("{mismatches:#?}\n{text}");
        }
        assert!(text.contains(
            "loadgen_calls_total{stream=\"main\",shape=\"basic_call\",class=\"ok\",chaos=\"clear\"} 0\n"
        ));
        assert!(
            text.contains(
                "loadgen_e2e_latency_seconds_count{stream=\"main\",shape=\"basic_call\"} 0\n"
            ),
            "a declared scenario's call latency is written from the start: {text}"
        );
        assert!(!text.contains("loadgen_e2e_seconds"), "no lifetime quantile gauge: {text}");
        assert!(text.contains(
            "loadgen_rtt_seconds_count{stream=\"main\",shape=\"basic_call\",exchange=\"invite_100\"} 1\n"
        ));
        // Every sample names its stream first (the cap's overflow counter is
        // the catalogue's own, unlabelled).
        for line in text.lines().filter(|l| !l.starts_with('#') && !l.is_empty()) {
            let unlabelled = line.starts_with("loadgen_mux_orphan_overflow_total ");
            assert!(unlabelled || line.contains("{stream=\"main\""), "no stream label: {line}");
        }
    }
}
