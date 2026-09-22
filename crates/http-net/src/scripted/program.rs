//! The program a [`ScriptedHttpService`](super::ScriptedHttpService) serves:
//! one [`HttpScript`] per expected conversation, stated as data.
//!
//! A script opens on a request matching [`HttpScript::open`]; its
//! [`HttpScriptStep`] answers that request and every follow-up that returns
//! the continuation token the previous reply carried. Matching is direct text:
//! each [`HttpRequestMatch::contains`] entry is a substring of the request
//! body (the body compact-re-serialised first when it parses as JSON).
//!
//! Templates (match entries, reply header values and bodies) accept exactly
//! three placeholders: `${bind:NAME}` (a value given at `add`),
//! `${capture:NAME}` (one JSON scalar captured from a request of the same
//! instance) and `${continuation}` (the token, replies only). Any other `${…}`
//! is refused at `add`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::HttpRequest;

/// One expected conversation: the request that opens it and the steps that
/// answer it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpScript {
    /// The request that opens an instance of this script (no token).
    pub open: HttpRequestMatch,
    /// What answers the opening request and its follow-ups.
    pub step: HttpScriptStep,
}

impl HttpScript {
    /// A script of reified steps: step 0 answers the opening request, step
    /// `n` the request that returns the token step `n - 1` minted.
    pub fn reified(open: HttpRequestMatch, steps: Vec<HttpReifiedStep>) -> Self {
        Self { open, step: HttpScriptStep::Reified(steps) }
    }

    /// A script whose steps are computed by `code` (see [`HttpScriptStep::Code`]).
    pub fn code<F>(open: HttpRequestMatch, code: F) -> Self
    where
        F: Fn(
                &HttpRequest,
                &HttpBindings,
                Option<&HttpState>,
            ) -> Result<(HttpReply, Option<HttpState>), HttpUnmatched>
            + Send
            + Sync
            + 'static,
    {
        Self { open, step: HttpScriptStep::Code(Arc::new(code)) }
    }
}

/// A request predicate: method, path (query ignored) and body fragments.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpRequestMatch {
    /// Request method, compared exactly (`"POST"`).
    pub method: String,
    /// Request path, compared exactly after the query string is cut off on
    /// both sides.
    pub path: String,
    /// Body fragments, each a template over `${bind:…}` and `${capture:…}`
    /// that must be a substring of the (compacted) body. One entry is one
    /// `"key":value` fragment; a span over two keys depends on the peer's key
    /// order and is not portable.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contains: Vec<String>,
}

impl HttpRequestMatch {
    /// A `method path` predicate with no body fragment.
    pub fn new(method: impl Into<String>, path: impl Into<String>) -> Self {
        Self { method: method.into(), path: path.into(), contains: Vec::new() }
    }

    /// A `POST path` predicate with no body fragment.
    pub fn post(path: impl Into<String>) -> Self {
        Self::new("POST", path)
    }

    /// Add one body fragment (builder style).
    pub fn contains(mut self, fragment: impl Into<String>) -> Self {
        self.contains.push(fragment.into());
        self
    }
}

/// The signature of a [`HttpScriptStep::Code`] step: the raw request, the
/// instance's bindings and its state (`None` on the opening request) in; the
/// reply and the next state (`None` = the instance is complete) out, or
/// [`HttpUnmatched`] when the request is not the one expected.
pub type HttpCode = dyn Fn(
        &HttpRequest,
        &HttpBindings,
        Option<&HttpState>,
    ) -> Result<(HttpReply, Option<HttpState>), HttpUnmatched>
    + Send
    + Sync;

/// How a script answers its requests.
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HttpScriptStep {
    /// A fixed sequence: step `n` answers the request at position `n`.
    Reified(Vec<HttpReifiedStep>),
    /// A closure deciding each reply from the request and the state the
    /// previous reply's token carried. Not serialisable: a program read from
    /// a file is reified.
    #[serde(skip)]
    Code(Arc<HttpCode>),
}

impl fmt::Debug for HttpScriptStep {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Reified(steps) => f.debug_tuple("Reified").field(steps).finish(),
            Self::Code(_) => f.write_str("Code(..)"),
        }
    }
}

/// One position of a reified script: what the request there must be, and the
/// reply.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HttpReifiedStep {
    /// The request expected at this position. Step 0's is checked on the
    /// opening request in addition to [`HttpScript::open`].
    pub expect: HttpRequestMatch,
    /// The reply to it.
    pub reply: HttpReply,
}

impl HttpReifiedStep {
    /// `reply` answers a request matching `expect`.
    pub fn new(expect: HttpRequestMatch, reply: HttpReply) -> Self {
        Self { expect, reply }
    }
}

/// What the service does with a request it matched.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HttpReply {
    /// A complete response. Header values and the body are templates.
    Respond {
        /// Status code; a stated 5xx is a response like any other.
        status: u16,
        /// Response headers, values templated.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        headers: Vec<(String, String)>,
        /// Response body template.
        #[serde(default)]
        body: String,
    },
    /// Never answer: the origin does not complete the response and the
    /// client's own timeout governs (RFC 9112 §9.5). Mints no token.
    Silence,
    /// Answer `then` after `ms` milliseconds.
    Late {
        /// Delay before `then`, in milliseconds.
        ms: u64,
        /// The reply given after the delay.
        then: Box<HttpReply>,
    },
    /// Close the connection without a response (RFC 9112 §9.6). Mints no
    /// token.
    Reset,
}

impl HttpReply {
    /// A response with `status` and `body`, no header.
    pub fn respond(status: u16, body: impl Into<String>) -> Self {
        Self::Respond { status, headers: Vec::new(), body: body.into() }
    }

    /// Add one header to a [`Respond`](Self::Respond) (builder style); other
    /// variants are returned unchanged.
    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        if let Self::Respond { headers, .. } = &mut self {
            headers.push((name.into(), value.into()));
        }
        self
    }

    /// `self`, given after `ms` milliseconds.
    pub fn late(self, ms: u64) -> Self {
        Self::Late { ms, then: Box::new(self) }
    }
}

/// The values `${bind:NAME}` resolves to, given once per instance at `add`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HttpBindings(BTreeMap<String, String>);

impl HttpBindings {
    /// No binding.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name` to `value` (builder style).
    pub fn bind(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    /// The value bound to `name`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }
}

/// The opaque state a [`HttpScriptStep::Code`] step threads through the
/// continuation token.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct HttpState(pub serde_json::Value);

/// A [`HttpScriptStep::Code`] step's refusal of a request: answered and
/// recorded exactly like a reified mismatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpUnmatched {
    /// Why the request is not the expected one.
    pub detail: String,
}

impl HttpUnmatched {
    /// A refusal stating `detail`.
    pub fn new(detail: impl Into<String>) -> Self {
        Self { detail: detail.into() }
    }
}
