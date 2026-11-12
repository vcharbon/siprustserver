//! The run-spec: the parts every lane reads (the driver-compiled overlay,
//! `PCAP2TEST_PIVOT_V3.md` §4.3, and what the scripted peers do to the SDP
//! they send), and the whole spec of the binary that runs with nothing
//! registered. A product's binary states its own lane blocks around the same
//! envelope.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use pivot_schema::bundle::{CheckDisposition, IdentityBindings};
use pivot_schema::known_bug::KnownBug;
use pivot_schema::scoping::CheckClass;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The driver-compiled overlay: the `RunConfig` fields only a §4.3 compiler (or
/// a hand-authored pilot spec) can state. Field semantics are
/// `pivot_schema::bundle::RunConfig`'s; the lane fills in the rest.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RunOverlay {
    #[serde(default)]
    pub timing_tolerance_ms: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub injected_headers: BTreeMap<String, String>,
    /// Explicit identity bindings; anything left unstated is allocated back
    /// from the document's `observed` values.
    #[serde(default, skip_serializing_if = "IdentityBindings::is_empty")]
    pub identities: IdentityBindings,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claim_numbers: BTreeMap<String, Vec<String>>,
    /// Endpoint id → the socket the lane binds it at, where the document's
    /// `observed` address is not usable (it collides with the lane's own
    /// sockets, or names a port the host cannot bind).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub endpoint_addresses: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub check_scoping: BTreeMap<CheckClass, CheckDisposition>,
    /// Defects the system this spec drives is known to produce, stated per
    /// CASE. What a lane's whole deployment carries belongs on its lane block;
    /// the two are unioned.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub known_bugs: BTreeSet<KnownBug>,
}

/// How a lane's scripted peers treat the SDP they send: `verbatim` (the
/// default) rides it byte for byte, since no lane replays media; `rewrite`
/// rebooks `c=`/`m=` onto the lane's own address and ports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum SdpPlane {
    #[default]
    Verbatim,
    Rewrite,
}

impl SdpPlane {
    /// Whether the peers ride the SDP as stored.
    pub fn is_verbatim(&self) -> bool {
        matches!(self, SdpPlane::Verbatim)
    }
}

/// The port book's base a lane rebooks RTP ports from under
/// [`SdpPlane::Rewrite`], where the lane states none.
pub const DEFAULT_RTP_BASE: u16 = 40_000;

fn default_rtp_base() -> u16 {
    DEFAULT_RTP_BASE
}

/// One run of the `pivot-replay` binary: a case, its lane, an output
/// directory, and the compiled run-configuration overlay (the envelope
/// `@sip/driver` emits).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct RunSpec {
    /// The v3 scenario document: a file, or a case directory holding
    /// `scenario.json`. Body resources resolve beside the document.
    pub case: PathBuf,
    /// Where the run bundle is written. Owned by the run: a previous run's
    /// content under it is removed.
    pub out_dir: PathBuf,
    pub lane: LaneSpec,
    #[serde(default)]
    pub run: RunOverlay,
}

impl RunSpec {
    /// The run-spec at `path`: unreadable is an environment failure, a text
    /// that is no run-spec a refusal.
    pub fn read(path: &Path) -> Result<RunSpec, crate::Failure> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| crate::Failure::Environment(format!("read {}: {e}", path.display())))?;
        serde_json::from_str(&text).map_err(|e| {
            crate::Failure::Refused(format!("{} is not a run-spec: {e}", path.display()))
        })
    }
}

/// The lanes the binary runs with nothing registered.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "kebab-case", deny_unknown_fields)]
pub enum LaneSpec {
    /// The plain system (`crate::plain`) in process, on a shared simulated
    /// fabric under a paused clock anchored at the epoch. Lint is advisory.
    Simulated(SimulatedBlock),
}

/// The simulated lane's knobs.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct SimulatedBlock {
    /// The `endpoints[].id` whose bound socket the system dials a b-leg at.
    pub egress_endpoint: String,
    /// The port book's base, read only under [`SdpPlane::Rewrite`].
    #[serde(default = "default_rtp_base")]
    pub rtp_base: u16,
    /// What the scripted peers do to the SDP they send.
    #[serde(default, skip_serializing_if = "SdpPlane::is_verbatim")]
    pub sdp: SdpPlane,
}
