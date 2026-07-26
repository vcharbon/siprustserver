//! The header model of ADR-0025: header identity as a value.
//!
//! A header's name is a [`HeaderName`], resolved once from the wire bytes —
//! RFC 3261 §7.3.3 compact forms and arbitrary casing collapse into the same
//! variant, and an extension header keeps its wire spelling in
//! [`HeaderName::Other`]. [`HeaderClass`] is the crate's single
//! structural/end-to-end classification: every "which headers does the stack
//! own" table is a query against it.

mod class;
mod name;

pub use class::HeaderClass;
pub use name::HeaderName;
