//! What a stub sends back for one request: after a delay, a response or a
//! connection closed without one.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::{HttpAnswer, HttpResponse};

/// One answer: `delay` of waiting, then the [`StubReply`].
///
/// JSON form: `{ "delay_ms": 0, "respond": { "status": 200, "headers": [[n, v]],
/// "body_b64": "…" } }`, or `{ "delay_ms": 0, "close": {} }`; `delay_ms`,
/// `headers` and `body_b64` default to none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubAnswer {
    /// How long the stub waits before it replies, from the request's arrival.
    #[serde(rename = "delay_ms", with = "millis", default)]
    pub delay: Duration,
    /// What it sends once the delay ran.
    #[serde(flatten)]
    pub reply: StubReply,
}

/// A response, or a close without one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StubReply {
    /// Send this status line, headers and body.
    Respond {
        /// The status code, any value the peer accepts on the wire.
        status: u16,
        /// Response headers, in order.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<(String, String)>,
        /// The body bytes as sent, never validated.
        #[serde(rename = "body_b64", with = "super::body", default)]
        body: Vec<u8>,
    },
    /// Close the connection without a response (RFC 9112 §9.6).
    Close {},
}

impl StubAnswer {
    /// `status` with `body`, at once.
    pub fn respond(status: u16, body: Vec<u8>) -> Self {
        Self {
            delay: Duration::ZERO,
            reply: StubReply::Respond { status, headers: Vec::new(), body },
        }
    }

    /// A close without a response, at once.
    pub fn close() -> Self {
        Self { delay: Duration::ZERO, reply: StubReply::Close {} }
    }

    /// The same answer with one more response header (no effect on a close).
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let StubReply::Respond { headers, .. } = &mut self.reply {
            headers.push((name.into(), value.into()));
        }
        self
    }

    /// The same answer sent after `delay`.
    pub fn delayed(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    /// What the transport sends once the delay ran.
    pub(super) fn to_http(&self) -> HttpAnswer {
        match &self.reply {
            StubReply::Respond { status, headers, body } => HttpAnswer::Response(
                HttpResponse::status(*status).with_headers(headers.clone()).with_body(body.clone()),
            ),
            StubReply::Close {} => HttpAnswer::Abort,
        }
    }
}

/// A [`Duration`] as whole milliseconds.
mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        delay: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_u64(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        u64::deserialize(deserializer).map(Duration::from_millis)
    }
}
