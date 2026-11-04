//! [`AdaptationHttpPort`]: the backend a service's async HTTP callback is
//! sent to.

use std::net::SocketAddr;
use std::sync::Arc;

/// Host-injected capability for the generic service-authorable async-HTTP
/// callback (`RuleAction::ServiceHttpRequest`). A logical endpoint path is
/// sent to the backend `client` names, under the generic retry rule
/// (ADR-0043): one attempt has `attempt`, every attempt together the request's
/// budget, its `timeout_ms` or `default_timeout`. `default_timeout` also sets
/// the answer deadline of a request that states no `timeout_ms` (ADR-0039).
/// With no port wired, a service that fires the effect gets an
/// `outcome:"error"` re-entry.
#[derive(Clone)]
pub struct AdaptationHttpPort {
    /// The backend every request goes to (binary-safe: `HttpRequest`/
    /// `HttpResponse` bodies are `Vec<u8>`).
    pub client: http_net::BackendClient,
    /// One attempt's budget, at most the request's.
    pub attempt: std::time::Duration,
    /// A request's budget, every attempt together, when the effect omits
    /// `timeout_ms`.
    pub default_timeout: std::time::Duration,
}

impl AdaptationHttpPort {
    /// A port to the backend at `base` over `transport`, with the default
    /// client settings and a breaker probing `/`; one attempt fills a
    /// request's budget, `default_timeout` when the effect states none.
    pub fn addressed(
        transport: Arc<dyn http_net::HttpTransport>,
        base: SocketAddr,
        default_timeout: std::time::Duration,
    ) -> Self {
        let client = http_net::BackendClient::new(
            transport,
            Arc::new(http_net::ResolvedTarget::addr(base)),
            http_net::ClientSettings::default(),
            http_net::BreakerConfig::probing("/"),
        );
        Self { client, attempt: default_timeout, default_timeout }
    }
}
