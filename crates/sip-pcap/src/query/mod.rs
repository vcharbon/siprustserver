//! Declarative queries over the flow model — the search phase, between
//! correlation and extraction.
//!
//! The three phases are deliberately separate: [`crate::flow`] decides which
//! legs are one call, this module decides which calls answer a question, and
//! [`Projection`] decides how much of a matching call to hand back — a few
//! named fields for a corpus screening pass, or the whole model with raw
//! payloads for extraction. Only the last is expensive, so a sweep pays for it
//! only on the calls it actually wants.
//!
//! JSON is the canonical query form ([`Query::from_json`]). A text query
//! language, if it ever exists, is a parser producing the same [`Node`] tree —
//! the evaluator has no opinion about syntax.
//!
//! ```text
//! { "name": "update-rejected",
//!   "scope":     { "time": {"from_us": 1638412000000000} },
//!   "correlate": { "strategies": [ {"derived_call_id": {}} ] },
//!   "select":    { "any_txn": { "all": [ {"method": "UPDATE"},
//!                                        {"final_status": {"ge": 400}} ] } },
//!   "project":   { "mode": "summary", "fields": ["t0_us", "ruri", "src", "dst"] } }
//! ```

pub mod ast;
pub mod eval;
pub mod load;
pub mod project;

pub use ast::{
    KeyField, Neighbours, Node, NumCmp, Projection, Query, Scope, StatusMatch, StrMatch,
};
pub use eval::{group_t0, group_t1, select_groups, Ctx};
pub use load::QueryError;
pub use project::{neighbours_of, summary_row};
