//! A stub HTTP service for tests that drive a real peer: each request is
//! answered by the first mapping it matches, and every request is recorded.
//!
//! - **Mappings.** A [`StubMapping`] matches a request on its method, its
//!   path, its query ([`StubQuery`]: any, the exact query string, or the
//!   parameters in any order) and, where it names one, the listener the
//!   request came in on. Mappings are tried in order; the first match answers.
//! - **Answers.** A [`StubAnswer`] waits its delay, then sends a status,
//!   headers and raw body bytes (never validated), or closes the connection
//!   without a response. A delay longer than the peer's timeout is a hang the
//!   peer ends.
//! - **Default.** A request no mapping matches gets the stub's default answer.
//! - **Record.** Every request is recorded as it arrives ([`StubRecord`]):
//!   its time, listener, method, path, query, headers, body, and the mapping
//!   that answered it.
//! - **Shutdown.** [`Stub::shutdown`] ends every pending answer, and every
//!   later one, with a close at once, so no delay outlives the stub.
//!
//! One [`Stub`] serves several listeners ([`Stub::listener`]), each bound by
//! the caller on any [`HttpTransport`](crate::HttpTransport), and keeps one
//! record across them.

mod answer;
mod body;
mod mapping;
mod query;
mod record;
mod service;

pub use answer::{StubAnswer, StubReply};
pub use mapping::{StubMapping, StubQuery};
pub use record::StubRecord;
pub use service::{Stub, StubSpec};
