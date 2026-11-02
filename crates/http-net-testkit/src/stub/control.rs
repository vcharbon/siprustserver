//! The stub's control service, bound apart from its listeners:
//!
//! - `PUT /sets/<tag>` with a JSON array of mappings registers them under
//!   `<tag>`, replacing a set of that tag: `204`.
//! - `DELETE /sets/<tag>` removes the set and every request it answered from
//!   the record: `204`. `DELETE /sets` removes every set so.
//! - `<tag>` is one path segment, percent-decoded (RFC 3986 §2.1).
//! - `GET /records[?set=<tag>][&since_ms=<ms>][&until_ms=<ms>]` answers the
//!   record as a JSON array, in arrival order: the requests the set answered,
//!   received from `since_ms` to `until_ms` (Unix milliseconds, both bounds
//!   included), every request where no filter is given.
//! - `GET /health` answers `{"sets": n, "records": n}`.
//! - `POST /shutdown` shuts the stub down ([`Stub::shutdown`]): `202`.
//!
//! A body or a bound that does not read is `400`, any other request `404`.

use async_trait::async_trait;

use super::mapping::StubMapping;
use super::query;
use super::service::Stub;
use crate::{HttpRequest, HttpResponse, HttpService};

/// The control service of one stub.
pub(super) struct Control {
    stub: Stub,
}

impl Control {
    pub(super) fn new(stub: Stub) -> Self {
        Self { stub }
    }

    fn records(&self, query: Option<&str>) -> HttpResponse {
        let (mut set, mut since, mut until) = (None, 0u64, u64::MAX);
        for (name, value) in query::params(query.unwrap_or("")) {
            match name.as_str() {
                "set" => set = Some(value),
                "since_ms" | "until_ms" => {
                    let Ok(ms) = value.parse::<u64>() else {
                        return bad(&format!("{name}={value} is no count of milliseconds"));
                    };
                    if name == "since_ms" {
                        since = ms;
                    } else {
                        until = ms;
                    }
                }
                _ => return bad(&format!("no filter is named {name}")),
            }
        }
        let kept: Vec<_> = self
            .stub
            .records()
            .into_iter()
            .filter(|r| set.is_none() || r.set == set)
            .filter(|r| r.at_unix_ms >= since && r.at_unix_ms <= until)
            .collect();
        json(&kept)
    }
}

fn bad(why: &str) -> HttpResponse {
    HttpResponse::status(400).with_body(why.as_bytes().to_vec())
}

fn json<T: serde::Serialize>(value: &T) -> HttpResponse {
    match serde_json::to_vec(value) {
        Ok(body) => HttpResponse::ok(body).header("content-type", "application/json"),
        Err(e) => HttpResponse::status(500).with_body(e.to_string().into_bytes()),
    }
}

#[async_trait]
impl HttpService for Control {
    async fn handle(&self, req: HttpRequest) -> HttpResponse {
        let (path, query) = query::split(&req.path);
        let tag = path
            .strip_prefix("/sets/")
            .filter(|tag| !tag.is_empty() && !tag.contains('/'))
            .map(query::segment);
        let tag = tag.as_deref();
        match (req.method.as_str(), path, tag) {
            ("PUT", _, Some(tag)) => match serde_json::from_slice::<Vec<StubMapping>>(&req.body) {
                Ok(mappings) => {
                    self.stub.register(tag, mappings);
                    HttpResponse::status(204)
                }
                Err(e) => bad(&format!("the body is no array of mappings: {e}")),
            },
            ("DELETE", _, Some(tag)) => {
                self.stub.clear(tag);
                HttpResponse::status(204)
            }
            ("DELETE", "/sets", None) => {
                self.stub.clear_all();
                HttpResponse::status(204)
            }
            ("GET", "/records", None) => self.records(query),
            ("GET", "/health", None) => json(&serde_json::json!({
                "sets": self.stub.sets(),
                "records": self.stub.records().len(),
            })),
            ("POST", "/shutdown", None) => {
                self.stub.shutdown();
                HttpResponse::status(202)
            }
            _ => HttpResponse::status(404),
        }
    }
}
