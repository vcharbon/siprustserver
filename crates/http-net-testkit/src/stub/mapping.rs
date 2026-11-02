//! A stub mapping: the request it matches, and its answer.

use serde::{Deserialize, Serialize};

use super::answer::StubAnswer;
use super::query;

/// One request → answer mapping.
///
/// JSON form: `{ "tag": "…", "listener": "…", "method": "GET", "path": "/v1/x",
/// "query": …, "answer": … }`; `tag`, `listener` and `query` are optional.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StubMapping {
    /// What the record names this mapping by when it answers a request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// The one listener it answers on; any where `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listener: Option<String>,
    /// The request method, compared exactly (`GET`).
    pub method: String,
    /// The request path, compared exactly, without the query.
    pub path: String,
    /// What the query must be.
    #[serde(default)]
    pub query: StubQuery,
    /// What a matched request gets.
    pub answer: StubAnswer,
}

/// What a mapping requires of the query.
///
/// JSON form: `"any"`, `{ "exact": "a=1&b=2" }`, or
/// `{ "params": [["a", "1"], ["b", "2"]] }`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StubQuery {
    /// Any query, or none.
    #[default]
    Any,
    /// The query string exactly as sent, without its `?`; `""` matches a
    /// target ending in `?` and one with no query at all.
    Exact(String),
    /// All and only these parameters, decoded, in any order; a name stated
    /// twice must come twice.
    Params(Vec<(String, String)>),
}

impl StubQuery {
    /// The query string `query` exactly.
    pub fn exact(query: impl Into<String>) -> Self {
        Self::Exact(query.into())
    }

    /// The parameters `pairs`, in any order.
    pub fn params<N: Into<String>, V: Into<String>>(
        pairs: impl IntoIterator<Item = (N, V)>,
    ) -> Self {
        Self::Params(pairs.into_iter().map(|(n, v)| (n.into(), v.into())).collect())
    }

    /// Whether the request's query (`None` where the target has no `?`) is one this accepts.
    fn accepts(&self, sent: Option<&str>) -> bool {
        match self {
            Self::Any => true,
            Self::Exact(want) => sent.unwrap_or("") == want,
            Self::Params(want) => {
                let mut got = query::params(sent.unwrap_or(""));
                let mut want = want.clone();
                got.sort();
                want.sort();
                got == want
            }
        }
    }
}

impl StubMapping {
    /// A mapping of `method path`, any query, any listener, untagged.
    pub fn new(method: impl Into<String>, path: impl Into<String>, answer: StubAnswer) -> Self {
        Self {
            tag: None,
            listener: None,
            method: method.into(),
            path: path.into(),
            query: StubQuery::Any,
            answer,
        }
    }

    /// The same mapping requiring `query`.
    pub fn query(mut self, query: StubQuery) -> Self {
        self.query = query;
        self
    }

    /// The same mapping answering on `listener` only.
    pub fn on_listener(mut self, listener: impl Into<String>) -> Self {
        self.listener = Some(listener.into());
        self
    }

    /// The same mapping, recorded as `tag`.
    pub fn tagged(mut self, tag: impl Into<String>) -> Self {
        self.tag = Some(tag.into());
        self
    }

    /// Whether a request to `target` (path and query) by `method` on
    /// `listener` is one this mapping answers.
    pub(super) fn matches(&self, listener: &str, method: &str, target: &str) -> bool {
        let (path, sent) = query::split(target);
        self.listener.as_deref().is_none_or(|own| own == listener)
            && self.method == method
            && self.path == path
            && self.query.accepts(sent)
    }
}
