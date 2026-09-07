//! **pivot-schema** — the wire contract for the replay pivot.
//!
//! A *pivot* is the declarative artifact between a packet capture and a
//! replayable test: which sockets to bind, which simulated elements sit on
//! them, and the choreography of every message. One format covers both ends of
//! that range — a captured call is the degenerate straight-line case, an
//! authored test uses the extension constructs, and one interpreter runs both.
//! This crate owns the schema. The prose companion is `PCAP2TEST_PIVOT_V3.md`;
//! where the two disagree, these structs govern.
//!
//! Five products, one per concern:
//!
//! - [`document::PivotV3`] and [`rules::RuleFile`] — serde + schemars models,
//!   `deny_unknown_fields` throughout, so a misspelled field fails loudly
//!   instead of silently changing a replay's meaning;
//! - [`bundle`] — the four record kinds one RUN leaves behind
//!   ([`bundle::RunConfig`], [`bundle::RunVerdict`], [`bundle::RunTiming`],
//!   [`bundle::RecordedMessage`]). The interpreter imports its own output
//!   shapes from here, so one crate owns every wire type and one binary emits
//!   every schema a mirror is checked against;
//! - [`canonical`] — the normative formatter (keys sorted lexically at every
//!   level, two-space indent, one trailing newline). Key order is the
//!   formatter's problem, never an emitter's;
//! - [`lint`] — the semantic rules a schema cannot state: every id resolves,
//!   every `alt` branch is discriminable, a captured document stays inside the
//!   generator subset, a CDR assertion is present or its absence is reasoned;
//! - [`tiers`] — the NORMATIVE DATA a pivot's tier model rests on: the tier-1
//!   omission list, per-message From/To/Contact handling, the compact-form
//!   header identity map and the multipart body-handling registry. Exported so
//!   every generator and interpreter reads one list.
//!
//! **Deployment-neutral by construction.** A pivot names numbering plans,
//! catalog classes, routing profiles, replay lanes, injector actions and rig
//! capabilities that belong to whatever platform is under test. Every such
//! field is modelled as an OPEN token or an open map here; this crate
//! enumerates only what SIP itself closes.

pub mod accessor;
pub mod body;
pub mod bundle;
pub mod call;
pub mod canonical;
pub mod case;
pub mod check;
pub mod deviation;
pub mod document;
pub mod flow;
pub mod identity;
pub mod known_bug;
pub mod lint;
pub mod msg;
pub mod must_fail;
pub mod placement;
pub mod postcondition;
pub mod rules;
pub mod schedules;
pub mod scoping;
pub mod tiers;
pub mod token;
pub mod violation;

pub use canonical::{format_json, format_str};
pub use document::{PIVOT_VERSION, PivotV3, Timing};
pub use lint::{Diagnostic, Report, Severity, lint};
pub use rules::{RULE_FILE_VERSION, RuleFile};
