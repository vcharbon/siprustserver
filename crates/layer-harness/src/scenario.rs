//! Scenario-level recording shapes (port of the SIP-agnostic half of
//! `report-recorder/types.ts` + `framework/types.ts`).
//!
//! The renderer-facing `RecordedScenario` here is intentionally **SIP-free**:
//! it carries lanes + anomalies only. The SIP-specific projection
//! (`RecordedSipEntry`, the `toSipWire` derivation) lives in `sip-net`, which
//! reads its own typed channel snapshot directly. Keeping SIP out of this
//! crate is what lets the cache / limiter / rules layers reuse it.

use std::collections::BTreeMap;
use std::net::SocketAddr;

use crate::anomaly::RecordedAnomaly;

/// A lane is one `(ip, port)` endpoint on the diagram. Keyed by [`LaneKey`].
pub type LaneKey = String;

/// Stable string key for an address. `SocketAddr`'s `Display` already renders
/// `ip:port` (and brackets IPv6), so this is just `addr.to_string()` — kept as
/// a named function to mirror the source's `laneKey(ip, port)` call sites.
pub fn lane_key(addr: SocketAddr) -> LaneKey {
    addr.to_string()
}

/// Which physical fabric a lane belongs to (`ext` = the public SIP socket,
/// `core` = the optional second fabric a proxy binds).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkTag {
    #[default]
    Ext,
    Core,
}

/// How the scenario's transport was wired. Recorded structurally so the
/// renderer can label the diagram and a test cannot misreport it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    Fake,
    Live,
    Hybrid,
}

/// A registered lane: its address, the name(s) it was labelled with, the
/// fabric it sits on, and any kill timestamps (virtual or wall ms).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lane {
    /// The registry key — the bare `ip:port`, or `ip:port#<label>` for a
    /// logical sub-lane on a shared socket (newkahneed-036 ask C). Matches the
    /// `bind_key` recorded on the signaling events of this lane's endpoint.
    pub key: LaneKey,
    pub addr: SocketAddr,
    pub names: Vec<String>,
    pub network: NetworkTag,
    pub killed_at: Vec<u64>,
}

/// The recorder's drained state at scenario end (SIP-free half). A layer that
/// needs SIP-level trace entries pairs this with its own channel snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordedScenario {
    pub transport_kind: TransportKind,
    pub lanes: Vec<Lane>,
    pub anomalies: Vec<RecordedAnomaly>,
}

/// Internal mutable lane state held by the [`crate::Recorder`].
#[derive(Debug, Clone)]
pub(crate) struct MutableLane {
    pub addr: SocketAddr,
    pub names: Vec<String>,
    pub network: NetworkTag,
    pub killed_at: Vec<u64>,
}

/// Lane registry: key → lane. Used inside the recorder; exposed via snapshot.
pub(crate) type LaneRegistry = BTreeMap<LaneKey, MutableLane>;
