//! A scripted [`HttpService`](crate::HttpService) for tests: the HTTP
//! exchanges a scenario expects, stated as a program and served under the
//! hard-failure rule.
//!
//! - **Program.** Each [`HttpScript`] opens on a request predicate and answers
//!   through reified steps or a code step. Bodies are templates bound late:
//!   `${bind:…}` values are given per instance at [`add`](ScriptedHttpService::add).
//! - **Continuation.** Position within an instance rides an opaque token the
//!   service mints into its replies (`${continuation}`) and scans back from
//!   the next request body; the service keeps no progress used for matching,
//!   so concurrent instances and retransmitted requests cannot interfere. The
//!   token may travel under a peer-specific wrapping ([`HttpContinuationCodec`],
//!   none by default). ADR-0036.
//! - **Faults.** A reply may be withheld ([`HttpReply::Silence`]), delayed
//!   ([`HttpReply::Late`]) or turned into a connection close
//!   ([`HttpReply::Reset`], through [`HttpService::answer`](crate::HttpService::answer)).
//! - **Hard failure.** A request no script states is answered `500` and
//!   recorded; an instance left short of its last step is unserved at run end
//!   ([`HttpScriptHandle::verdict`]).

mod codec;
mod error;
mod matcher;
mod open;
mod program;
mod service;
mod template;
mod token;
mod validate;
mod verdict;

pub use codec::{HttpContinuationCodec, IdentityCodec};
pub use error::HttpScriptError;
pub use program::{
    HttpBindings, HttpCode, HttpReifiedStep, HttpReply, HttpRequestMatch, HttpScript,
    HttpScriptStep, HttpState, HttpUnmatched,
};
pub use service::ScriptedHttpService;
pub use verdict::{HttpFinding, HttpFindingKind, HttpScriptHandle, HttpVerdict};
