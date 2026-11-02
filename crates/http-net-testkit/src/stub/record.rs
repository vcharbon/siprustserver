//! One request as the stub received it.

use std::time::Duration;

use serde::Serialize;

/// A request the stub received, recorded on its arrival, before any delay.
///
/// JSON form: `{ "at_ms", "at_unix_ms", "listener", "method", "path",
/// "query", "headers", "body_b64", "matched", "tag" }`; `query` is absent
/// where the target had no `?`, `tag` where no tagged mapping answered.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct StubRecord {
    /// When it arrived, from the stub's creation, on the runtime's clock.
    #[serde(rename = "at_ms", serialize_with = "millis")]
    pub at: Duration,
    /// When it arrived, in milliseconds since the Unix epoch, on the system clock.
    pub at_unix_ms: u64,
    /// The listener it came in on.
    pub listener: String,
    /// The request method.
    pub method: String,
    /// The target's path, without the query.
    pub path: String,
    /// The query string as sent, without its `?`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// The request headers as the transport handed them over.
    pub headers: Vec<(String, String)>,
    /// The body bytes as received.
    #[serde(rename = "body_b64", serialize_with = "super::body::serialize")]
    pub body: Vec<u8>,
    /// Whether a mapping answered it; `false` for the default answer.
    pub matched: bool,
    /// The tag of the mapping that answered it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

fn millis<S: serde::Serializer>(at: &Duration, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_u64(u64::try_from(at.as_millis()).unwrap_or(u64::MAX))
}
