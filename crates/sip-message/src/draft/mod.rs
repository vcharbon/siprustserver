//! The construction seam of ADR-0025: thaw / edit / freeze.
//!
//! A parsed message and a message under construction are a persistent/transient
//! pair. [`Draft`] is the thawed, editable twin; `freeze` is the only way to
//! make a message and `thaw` the only way to open one. Origination and relay
//! are the same type in different starting states — there is no separate
//! editor, and no path that mutates a message in place.
//!
//! One engine serves both directions: the entry list, the functional updates,
//! the list views and the render pass are written once, and
//! [`StartKind`](start::StartKind) contributes only the start line, the
//! mandatory header set and the frozen message type.

mod edit;
mod entry;
mod list;
mod render;
mod seed;
mod start;

pub use edit::{Draft, IncompleteDraft};
pub use entry::Entry;
pub use list::HeaderList;
pub use seed::{RequestDraft, ResponseDraft};
pub use start::{kind, RequestLine, Span, StartKind, StartSpans, StatusLine, SIP_VERSION};

#[cfg(test)]
mod tests;
