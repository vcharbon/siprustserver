//! Operator routes: every path under `/admin/` goes to the runner's
//! [`AdminFn`], which reads the method and answers with an [`AdminReply`];
//! this module frames the reply.

use std::sync::Arc;

/// The prefix of every operator route.
pub(crate) const PREFIX: &str = "/admin/";

/// Operator routes, called with the request's method and path (query
/// included) for every path under `/admin/`; `None` ⇒ `404`. Cheap and
/// non-blocking, like every probe route.
pub type AdminFn = Arc<dyn Fn(&str, &str) -> Option<AdminReply> + Send + Sync>;

/// An operator route's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminReply {
    /// The HTTP status code.
    pub status: u16,
    /// The JSON body; empty for none.
    pub json: String,
    /// The `Allow` header a `405` states.
    pub allow: Option<&'static str>,
}

impl AdminReply {
    /// `204 No Content`: done, nothing to say.
    pub fn no_content() -> Self {
        Self { status: 204, json: String::new(), allow: None }
    }

    /// `status` with a JSON body.
    pub fn json(status: u16, json: String) -> Self {
        Self { status, json, allow: None }
    }

    /// `405 Method Not Allowed`, stating the methods the route serves.
    pub fn method_not_allowed(allow: &'static str) -> Self {
        Self { status: 405, json: String::new(), allow: Some(allow) }
    }

    /// The reply as an HTTP/1.1 response that closes its connection. A `204`
    /// carries no body and no `Content-Length` (RFC 9110 §8.6).
    pub(crate) fn render(&self) -> String {
        let mut head = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
        if let Some(allow) = self.allow {
            head.push_str(&format!("Allow: {allow}\r\n"));
        }
        if self.status != 204 {
            if !self.json.is_empty() {
                head.push_str("Content-Type: application/json\r\n");
            }
            head.push_str(&format!("Content-Length: {}\r\n", self.json.len()));
        }
        head.push_str("Connection: close\r\n\r\n");
        if self.status != 204 {
            head.push_str(&self.json);
        }
        head
    }
}

/// The reason phrase of the codes an operator route answers.
fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    }
}
