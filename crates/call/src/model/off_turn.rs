//! A request the call sent off its turn to an outside service, kept on the
//! record while its answer is awaited (ADR-0039, ADR-0055 §1): the task that
//! awaits the answer lives on the node that sent the request and dies with
//! it, so whichever node serves the call next sends the request again from
//! this image. The image is the request as its turn sent it; the change
//! numbers of an admit it carries are the sending node's, and a node sending
//! it again numbers its own.

use serde::{Deserialize, Serialize};

use super::limiter::{ConsultAdmits, LimiterEntry};

/// One request awaiting its answer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum OffTurnRequest {
    /// The REFER's authorization, its request as the seed rule built it, sent
    /// at `sent_at_ms`: it has no answer deadline of its own.
    Refer { request: serde_json::Value, sent_at_ms: i64 },
    /// A service's adaptation HTTP request.
    ServiceHttp {
        correlation_id: String,
        endpoint: String,
        method: String,
        headers: Vec<(String, String)>,
        #[serde(with = "serde_bytes")]
        body: Vec<u8>,
        content_type: Option<String>,
        timeout_ms: Option<u64>,
    },
    /// A `/call/failure` consult, its request as the seed rule built it and
    /// the admits its chain may send.
    Failure { request: serde_json::Value, limiter: ConsultAdmits },
    /// A release event's consult, its request as the rule built it and the
    /// admit its reroute may send.
    Release { request: serde_json::Value, limiter: ConsultAdmits },
    /// A service's replacement of the call's admission set, numbered
    /// `change` by the turn that sent it.
    Admit { correlation_id: String, entries: Vec<LimiterEntry>, change: u64 },
}

impl OffTurnRequest {
    /// Whether `other` is the same request: a service's by its correlation
    /// id, a consult by the first change number it reserved, the REFER's
    /// authorization by its kind alone (one is awaited at a time).
    pub fn same_request(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Refer { .. }, Self::Refer { .. }) => true,
            (
                Self::ServiceHttp { correlation_id: a, .. },
                Self::ServiceHttp { correlation_id: b, .. },
            )
            | (Self::Admit { correlation_id: a, .. }, Self::Admit { correlation_id: b, .. }) => {
                a == b
            }
            (Self::Failure { limiter: a, .. }, Self::Failure { limiter: b, .. })
            | (Self::Release { limiter: a, .. }, Self::Release { limiter: b, .. }) => {
                a.first_change == b.first_change
            }
            _ => false,
        }
    }
}
