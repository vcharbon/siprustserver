//! The **run configuration**: everything a lane compiled for one run
//! (`PCAP2TEST_PIVOT_V3.md` §4.3).
//!
//! The driver lowers `calls` — attempt order, causes, dwells, provisional
//! profile, provisioning, number allocation — into whatever its lane needs, and
//! hands the interpreter this. **No lane semantics live inside the
//! interpreter**: it reads a lane NAME it never interprets, a clock mode, the
//! headers to stamp on its own sends, the route target its sends go through, and
//! the identity binding `${num:…}` resolves against.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::bundle::bindings::IdentityBindings;
use crate::known_bug::KnownBug;
use crate::scoping::CheckClass;

/// A tolerance nobody stated is not serialized: an absent window reads as the
/// exact value, which is what a run that says nothing about timing accepts.
fn is_zero(ms: &u64) -> bool {
    *ms == 0
}

/// Whether the run's clock is virtual (a paused runtime, where a compressible
/// dwell may be jumped) or real.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ClockMode {
    /// A paused runtime: a dwell the document marks `compressible` costs no wall
    /// time. A `timer_linked` dwell is still measured, because compressing what a
    /// system timer measures changes what the test proves.
    Virtual,
    /// Wall time: every dwell is slept.
    Real,
}

impl ClockMode {
    /// Whether this clock may compress `compressible`.
    pub fn compresses(self) -> bool {
        matches!(self, ClockMode::Virtual)
    }
}

/// What the run's media plane did to the session descriptions it sent
/// (`PCAP2TEST_PIVOT_V3.md` §8.3): whether the lane REBOOKED the lane-owned
/// tokens a body states — every `c=` address and every active `m=` port taken
/// from the lane's booking — or left every session description VERBATIM, the
/// tokens read as the body's content label only. A lane that exercises no
/// media runs verbatim, so what its peers relay is the captured description
/// byte for byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MediaMode {
    /// The lane's booking wrote its address and ports into every body stating
    /// the tokens.
    #[default]
    Rebooked,
    /// No body was rewritten: every session description rode as stored.
    Verbatim,
}

impl MediaMode {
    /// The default, and so the reading of a bundle that states nothing.
    fn is_rebooked(&self) -> bool {
        matches!(self, MediaMode::Rebooked)
    }
}

/// Wall time a run may burn ON TOP of the timeline its document declares: the
/// allowance for its own overhead, and the whole ceiling for a clock that
/// spends no wall time on the timeline itself.
const WALL_MARGIN_MS: u64 = 120_000;

/// What a check class costs on one run (`PCAP2TEST_PIVOT_V3.md` §9.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CheckDisposition {
    /// The check decides the run's status, like an unclassified one.
    Gating,
    /// The check is evaluated and recorded in the verdict's informative
    /// section, and the run's status does not turn on it.
    Informative,
}

/// One run's lane-compiled configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RunConfig {
    /// The lane's own name. Echoed into the bundle and never interpreted.
    pub lane: String,
    pub clock: ClockMode,
    /// Headers the lane stamps on every message the interpreter SENDS — a lane
    /// artifact (a test-correlation header, a forced egress hint), never part of
    /// the captured choreography. DOCUMENT-level: what the lane states once for
    /// the whole run.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub injected_headers: BTreeMap<String, String>,
    /// Call id (`calls[].id`) → the headers the lane stamps on THAT call's dial:
    /// the INVITE that opens its caller leg, and no other message.
    ///
    /// A directive that steers egress — the destination a call is routed to, the
    /// route plan it hunts, the admission entry it is counted against — is a
    /// per-call fact, so a two-call document states two of them. Stamping one
    /// call's destination on its neighbour's dial delivers a call to the wrong
    /// actor, which is why this is keyed and not merged.
    ///
    /// A call-level header WINS over the run-level header of the same name, and
    /// a header the DOCUMENT states wins over both.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub call_headers: BTreeMap<String, BTreeMap<String, String>>,
    /// The ± window, in milliseconds, this run accepts around a timer-anchored
    /// dwell (`PCAP2TEST_PIVOT_V3.md` §9.2).
    ///
    /// It reads ONE kind of fact: how long the system's own timer ran before it
    /// emitted (a `timer_linked` delay, §6.8). It never widens an ORDER and never
    /// softens a COUNT — a message that arrives out of order or does not arrive
    /// fails whatever the tolerance says. Zero, the default, accepts only the
    /// declared value.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub timing_tolerance_ms: u64,
    /// Where a leg's out-of-dialog request is addressed: the system under test's
    /// ingress, as `host:port`. In-dialog requests follow the learned remote
    /// target instead.
    pub route_target: String,
    /// The identity binding `${num:…}` resolves against.
    #[serde(default, skip_serializing_if = "IdentityBindings::is_empty")]
    pub identities: IdentityBindings,
    /// Identity name → the Request-URI user parts a `ruri-pos` claim may match
    /// when the system egresses that identity's leg back to us.
    ///
    /// A lane that relays the dialled number untouched states nothing here and
    /// the bound dial forms answer. A lane that dials one form and egresses
    /// another — a trunk-composed Request-URI, a plan that rewrites on the way
    /// out — states the egress form, because only the lane knows it. The
    /// interpreter never derives it: guessing which of two numbers a claim meant
    /// is how a call gets delivered to the wrong actor.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claim_numbers: BTreeMap<String, Vec<String>>,
    /// Endpoint id → the address the lane bound it at, where the lane binds by
    /// address rather than by the endpoint's `observed` value.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoint_addresses: BTreeMap<String, String>,
    /// Per-class overrides of the built-in downgrade: the lane states outright
    /// what a class costs, in either direction. A lane that shares the origin
    /// platform's CDR vocabulary without sharing its name states `gating`; a
    /// lane replaying its OWN document under a foreign header profile states
    /// `informative`. Anything more conditional than one word per class is a
    /// pre-processing step over the document, never interpreter smarts.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub check_scoping: BTreeMap<CheckClass, CheckDisposition>,
    /// Defects this lane's SUT is KNOWN to produce (`known_bug`): the gate each
    /// one names stands down, so the run reaches the steps behind the symptom
    /// instead of abandoning the leg on it. Never an acceptance — what the run
    /// then observes is recorded and classified like any other difference — and
    /// a lane that states none gates on everything.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub known_bugs: BTreeSet<KnownBug>,
    /// What the media plane did to the session descriptions this run sent.
    /// The interpreter states it from the lane's booking as it arms the bundle,
    /// so the record reads what the render applied rather than what a lane
    /// claimed; omitted at the default, which is how a bundle predating the
    /// field reads.
    #[serde(default, skip_serializing_if = "MediaMode::is_rebooked")]
    pub media: MediaMode,
}

impl RunConfig {
    /// A configuration naming a lane, its clock and its route target. Headers,
    /// identities and endpoint addresses are added by the builders below.
    pub fn new(lane: impl Into<String>, clock: ClockMode, route_target: impl Into<String>) -> Self {
        RunConfig {
            lane: lane.into(),
            clock,
            injected_headers: BTreeMap::new(),
            call_headers: BTreeMap::new(),
            timing_tolerance_ms: 0,
            route_target: route_target.into(),
            identities: IdentityBindings::new(),
            claim_numbers: BTreeMap::new(),
            endpoint_addresses: BTreeMap::new(),
            check_scoping: BTreeMap::new(),
            known_bugs: BTreeSet::new(),
            media: MediaMode::Rebooked,
        }
    }

    /// State what the media plane did to this run's session descriptions.
    pub fn with_media(mut self, media: MediaMode) -> Self {
        self.media = media;
        self
    }

    /// State what a check class costs on this lane, whatever the origin lane
    /// says.
    pub fn with_check_scoping(mut self, class: CheckClass, how: CheckDisposition) -> Self {
        self.check_scoping.insert(class, how);
        self
    }

    /// Declare a defect this lane's SUT is known to produce, standing the gate
    /// that names it down for this run.
    pub fn with_known_bug(mut self, bug: KnownBug) -> Self {
        self.known_bugs.insert(bug);
        self
    }

    /// Whether this lane declared `bug`.
    pub fn waives(&self, bug: KnownBug) -> bool {
        self.known_bugs.contains(&bug)
    }

    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.injected_headers.insert(name.into(), value.into());
        self
    }

    /// State a header on ONE call's dial, by the document's own `calls[].id`.
    pub fn with_call_header(
        mut self,
        call: impl Into<String>,
        name: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.call_headers.entry(call.into()).or_default().insert(name.into(), value.into());
        self
    }

    /// The headers this run stamps on `call`'s dial. Empty for a call the lane
    /// directs nowhere in particular.
    pub fn headers_for_call(&self, call: &str) -> Option<&BTreeMap<String, String>> {
        self.call_headers.get(call)
    }

    /// State the ± window this run accepts around a timer-anchored dwell (§9.2).
    pub fn with_timing_tolerance(mut self, ms: u64) -> Self {
        self.timing_tolerance_ms = ms;
        self
    }

    /// Whether this run accepts `observed_ms` for a dwell the document declares
    /// at `declared_ms`.
    ///
    /// Symmetric: a timer that fired EARLY is as far off as one that fired late,
    /// and a lane whose timers drift in one direction still states one window.
    pub fn absorbs_timing(&self, declared_ms: u64, observed_ms: u64) -> bool {
        observed_ms.abs_diff(declared_ms) <= self.timing_tolerance_ms
    }

    /// Wall time this run may burn before a loop that is not progressing is
    /// declared stuck, for a document declaring `declared_span_ms` of timeline
    /// and `settle_budget_ms` of settle.
    ///
    /// A paused clock JUMPS the declared dwells, so its wall time measures
    /// overhead alone and one flat margin bounds it. A real clock SLEEPS them,
    /// so the same margin sits on top of the declared timeline and the settle
    /// budget: a long capture replays to its end, and a run that hangs still
    /// reaches a ceiling instead of being waited on.
    pub fn wall_ceiling_ms(&self, declared_span_ms: u64, settle_budget_ms: u64) -> u64 {
        match self.clock {
            ClockMode::Virtual => WALL_MARGIN_MS,
            ClockMode::Real => {
                declared_span_ms.saturating_add(settle_budget_ms).saturating_add(WALL_MARGIN_MS)
            }
        }
    }

    pub fn with_identities(mut self, identities: IdentityBindings) -> Self {
        self.identities = identities;
        self
    }

    /// State the Request-URI user part a `ruri-pos` claim on `identity` matches.
    pub fn with_claim_number(
        mut self,
        identity: impl Into<String>,
        user: impl Into<String>,
    ) -> Self {
        self.claim_numbers.entry(identity.into()).or_default().push(user.into());
        self
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>, addr: impl Into<String>) -> Self {
        self.endpoint_addresses.insert(endpoint.into(), addr.into());
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_run_config_round_trips_through_its_bundle_form() {
        let config = RunConfig::new("upstream-fake", ClockMode::Virtual, "127.0.0.1:5080")
            .with_header("X-Run", "1")
            .with_identities(IdentityBindings::new().bind("caller", "private", "0009001"));
        let text = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<RunConfig>(&text).unwrap(), config);
        assert!(ClockMode::Virtual.compresses());
        assert!(!ClockMode::Real.compresses());

        // §2.2: a lane that bound nothing writes no map, like every other
        // collection here.
        let unbound = RunConfig::new("upstream-fake", ClockMode::Real, "h:1");
        let text = serde_json::to_string(&unbound).unwrap();
        assert!(!text.contains("identities"), "{text}");
        assert_eq!(serde_json::from_str::<RunConfig>(&text).unwrap(), unbound);
    }

    /// The media mode is written only when it is not the default, and a bundle
    /// stating none reads as rebooked.
    #[test]
    fn the_media_mode_is_omitted_at_its_default_and_read_back_otherwise() {
        let rebooked = RunConfig::new("upstream-fake", ClockMode::Virtual, "h:1");
        let text = serde_json::to_string(&rebooked).unwrap();
        assert!(!text.contains("media"), "{text}");
        assert_eq!(serde_json::from_str::<RunConfig>(&text).unwrap().media, MediaMode::Rebooked);

        let verbatim = rebooked.with_media(MediaMode::Verbatim);
        let text = serde_json::to_string(&verbatim).unwrap();
        assert!(text.contains(r#""media":"verbatim""#), "{text}");
        assert_eq!(serde_json::from_str::<RunConfig>(&text).unwrap(), verbatim);
    }

    /// What `check_scoping` MEANS is the interpreter's (`Scope`); this crate
    /// only owes it the wire form.
    #[test]
    fn a_check_scoping_override_survives_the_bundle_form() {
        let home = RunConfig::new("origin-platform", ClockMode::Virtual, "h:1")
            .with_check_scoping(CheckClass::OriginPlatformHeader, CheckDisposition::Informative);
        let text = serde_json::to_string(&home).unwrap();
        assert_eq!(serde_json::from_str::<RunConfig>(&text).unwrap(), home);
    }

    #[test]
    fn a_timing_tolerance_accepts_either_side_of_the_declared_dwell_and_nothing_wider() {
        let exact = RunConfig::new("upstream-fake", ClockMode::Virtual, "h:1");
        assert!(exact.absorbs_timing(15_000, 15_000));
        assert!(!exact.absorbs_timing(15_000, 15_001), "a run stating nothing accepts nothing");

        let lane =
            RunConfig::new("deployed-backend", ClockMode::Real, "h:1").with_timing_tolerance(700);
        assert!(lane.absorbs_timing(15_000, 15_700), "late, at the edge");
        assert!(lane.absorbs_timing(15_000, 14_300), "early, at the edge");
        assert!(!lane.absorbs_timing(15_000, 15_701));
        assert!(!lane.absorbs_timing(15_000, 14_299));
        // And it survives the bundle form.
        let text = serde_json::to_string(&lane).unwrap();
        assert_eq!(serde_json::from_str::<RunConfig>(&text).unwrap(), lane);
        assert!(text.contains("\"timing_tolerance_ms\":700"), "{text}");
        // A run that states none does not carry the field at all.
        assert!(!serde_json::to_string(&exact).unwrap().contains("timing_tolerance_ms"));
    }

    /// The ceiling is a HANG detector; on a real clock the timeline the document
    /// declares is not a hang.
    #[test]
    fn a_real_clock_wall_ceiling_follows_the_declared_timeline_and_a_paused_one_ignores_it() {
        let paused = RunConfig::new("upstream-fake", ClockMode::Virtual, "h:1");
        let real = RunConfig::new("upstream-real", ClockMode::Real, "h:1");

        // A paused clock jumps the dwells, so nothing the document declares
        // moves what it may burn.
        assert_eq!(paused.wall_ceiling_ms(0, 32_000), paused.wall_ceiling_ms(282_000, 32_000));

        // A real clock sleeps them: the longest capture in the corpus clears its
        // own span and its settle budget, and a longer one gets a wider ceiling.
        assert!(real.wall_ceiling_ms(282_000, 32_000) > 282_000 + 32_000);
        assert!(real.wall_ceiling_ms(282_000, 32_000) > real.wall_ceiling_ms(119_000, 32_000));

        // A document declaring no timeline is held to the same margin on either
        // clock, so a run that hangs before it speaks is still diagnosed.
        assert_eq!(real.wall_ceiling_ms(0, 0), paused.wall_ceiling_ms(0, 0));
        assert!(real.wall_ceiling_ms(0, 0) > 0);
    }

    #[test]
    fn a_directive_is_stated_per_call_and_a_neighbour_never_inherits_it() {
        let config = RunConfig::new("upstream-fake", ClockMode::Virtual, "h:1")
            .with_header("X-Run", "1")
            .with_call_header("c1", "X-Api-Call", "{\"destination\":\"bob\"}")
            .with_call_header("c2", "X-Api-Call", "{\"destination\":\"idle\"}");
        assert_eq!(
            config.headers_for_call("c1").and_then(|h| h.get("X-Api-Call")).map(String::as_str),
            Some("{\"destination\":\"bob\"}")
        );
        assert_eq!(
            config.headers_for_call("c2").and_then(|h| h.get("X-Api-Call")).map(String::as_str),
            Some("{\"destination\":\"idle\"}"),
            "the second call is directed at its OWN callee"
        );
        assert!(config.headers_for_call("c3").is_none(), "a call the lane directs nowhere");
        // The run-level header is the document's, and stays whole-run.
        assert_eq!(config.injected_headers.get("X-Run").map(String::as_str), Some("1"));
        let text = serde_json::to_string(&config).unwrap();
        assert_eq!(serde_json::from_str::<RunConfig>(&text).unwrap(), config);
    }

    #[test]
    fn an_unknown_run_config_field_is_refused_rather_than_ignored() {
        // A mistyped lane knob must not be silently dropped into a default.
        let text = r#"{"lane":"upstream-fake","clock":"virtual","route_target":"h:1","lame":true}"#;
        let strict: Result<RunConfig, _> = serde_json::from_str(text);
        assert!(strict.is_err(), "unknown field accepted: {strict:?}");
    }
}
