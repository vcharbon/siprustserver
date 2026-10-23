//! What a run learns from a [`ScriptedHttpService`](super::ScriptedHttpService):
//! the findings it recorded while serving and the per-instance verdict read
//! at run end.

use std::sync::Arc;

use serde::Serialize;

use super::service::Shared;

/// The class of a finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpFindingKind {
    /// A request no script states: no open matched and no token, the request
    /// at a token's position is not the expected one, or it comes past the
    /// last step. Answered `500`.
    Unmatched,
    /// Several scripts claim the request: incomparable overlapping opens, or
    /// two valid tokens in one body. Answered `500`.
    Ambiguous,
    /// At run end, an instance whose last step was never reached.
    Unserved,
    /// A `Reset` reached [`HttpService::handle`](crate::HttpService::handle),
    /// which cannot close a connection: the caller does not forward
    /// [`answer`](crate::HttpService::answer). Answered `500`.
    ResetNotForwarded,
    /// A valid token minted by another service (a straggler of an earlier
    /// run against a standing peer). Answered `500`; advisory.
    ForeignToken,
}

/// One finding, with the request that caused it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HttpFinding {
    /// Its class.
    pub kind: HttpFindingKind,
    /// The instances it concerns: none when no script claims the request,
    /// several for an ambiguity.
    pub instances: Vec<u64>,
    /// The request's method (empty for [`HttpFindingKind::Unserved`]).
    pub method: String,
    /// The request's path-and-query (empty for [`HttpFindingKind::Unserved`]).
    pub path: String,
    /// The request's body, lossily decoded.
    pub body: String,
    /// Expected against received, in words.
    pub detail: String,
}

impl HttpFinding {
    /// An advisory finding is reported but does not fail the run.
    pub fn is_advisory(&self) -> bool {
        self.kind == HttpFindingKind::ForeignToken
    }
}

/// One instance's verdict.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HttpVerdict {
    /// The instance.
    pub instance: u64,
    /// Whether a request opened it.
    pub opened: bool,
    /// Whether its last step was reached.
    pub complete: bool,
    /// Every finding concerning it, [`HttpFindingKind::Unserved`] included
    /// when it is not complete.
    pub findings: Vec<HttpFinding>,
}

impl HttpVerdict {
    /// No gating finding.
    pub fn is_green(&self) -> bool {
        self.findings.iter().all(HttpFinding::is_advisory)
    }
}

/// The caller's handle on one added script instance.
#[derive(Clone)]
pub struct HttpScriptHandle {
    pub(super) shared: Arc<Shared>,
    pub(super) instance: u64,
    pub(super) attributable: bool,
}

impl HttpScriptHandle {
    /// The instance id its tokens carry.
    pub fn instance(&self) -> u64 {
        self.instance
    }

    /// Whether its open states at least one `${bind:…}` fragment. Only such
    /// an instance can be told apart from its siblings by the request that
    /// opens it; without one, racing openers bind in arrival order.
    pub fn is_attributable(&self) -> bool {
        self.attributable
    }

    /// The verdict so far; read at run end for the final one.
    pub fn verdict(&self) -> HttpVerdict {
        self.shared.verdict(self.instance)
    }
}
