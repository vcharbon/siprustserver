//! The `reqwest` client side of the real transport: building a client from
//! [`ClientSettings`], sending one request on it, and [`NamedClient`], one
//! backend's client.
//!
//! A [`NamedClient`]'s URL carries its target as configured, so `Host` is the
//! backend's name, and every name lookup of the client goes through the
//! target ([`super::reqwest_resolve`]): bounded, single-flight, every address
//! returned, dropped when the target forgets it.

use std::sync::Arc;

use async_trait::async_trait;

use super::{HttpClient, HttpError, HttpRequest, HttpResponse, MAX_BODY_BYTES};
use crate::failures::FailureCause;
use crate::resolved_target::ResolvedTarget;
use crate::settings::ClientSettings;

/// A pooled `http://` client under `settings`, its name lookups going
/// through `target` when one is given. It follows no redirect: a 3xx is the
/// endpoint's answer, returned to the caller as its status.
pub(super) fn build(
    settings: &ClientSettings,
    target: Option<Arc<ResolvedTarget>>,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .http1_only()
        .connect_timeout(settings.connect_timeout)
        .timeout(settings.ceiling)
        .pool_idle_timeout(settings.pool_idle_timeout)
        .pool_max_idle_per_host(settings.max_idle_per_host)
        .tcp_keepalive(settings.keepalive_idle)
        .tcp_keepalive_interval(settings.keepalive_interval)
        .tcp_keepalive_retries(settings.keepalive_retries);
    #[cfg(any(target_os = "android", target_os = "fuchsia", target_os = "linux"))]
    {
        builder = builder.tcp_user_timeout(settings.user_timeout);
    }
    if let Some(target) = target {
        builder = builder.dns_resolver(Arc::new(super::reqwest_resolve::TargetResolve(target)));
    }
    builder.build().expect("reqwest client builds with a no-tls config")
}

/// Send `req` to `url` on `client`; a failure is counted under `peer` by its
/// cause and returned classified. A response body past [`MAX_BODY_BYTES`],
/// declared or streamed, fails [`FailureCause::BodyTooLarge`] once the cap is
/// passed, the rest left unread.
pub(super) async fn send(
    client: &reqwest::Client,
    peer: &str,
    url: &str,
    req: HttpRequest,
) -> Result<HttpResponse, HttpError> {
    let failed = |e: reqwest::Error| {
        // Cause-labelled counter only — the caller's episode owns the log
        // line (ADR-0026: no per-request output).
        let cause = super::cause::classify(&e);
        crate::failures::record(peer, cause);
        HttpError::new(peer, cause, e.to_string())
    };
    let method = reqwest::Method::from_bytes(req.method.as_bytes())
        .map_err(|e| HttpError::new(peer, FailureCause::Other, format!("bad method: {e}")))?;
    let mut builder = client.request(method, url);
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let too_large = || {
        crate::failures::record(peer, FailureCause::BodyTooLarge);
        HttpError::new(
            peer,
            FailureCause::BodyTooLarge,
            format!("response body past {MAX_BODY_BYTES} bytes"),
        )
    };
    let mut resp = builder.body(req.body).send().await.map_err(failed)?;
    let declared = resp.content_length().map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    if declared.is_some_and(|n| n > MAX_BODY_BYTES) {
        return Err(too_large());
    }
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let mut body = Vec::with_capacity(declared.unwrap_or(0));
    while let Some(chunk) = resp.chunk().await.map_err(failed)? {
        if chunk.len() > MAX_BODY_BYTES - body.len() {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(HttpResponse { status, headers, body })
}

/// One backend's `reqwest` client: its own pool, its URL naming the target.
/// A request resolves the target before `reqwest` connects, so a lookup is
/// bounded by the target's lookup bound, never cut by the connect timeout,
/// and a name that does not resolve fails as one ([`FailureCause::Dns`]).
///
/// [`FailureCause::Dns`]: crate::failures::FailureCause::Dns
pub(super) struct NamedClient {
    client: reqwest::Client,
    target: Arc<ResolvedTarget>,
    /// `http://<target>`, the authority as configured.
    base: String,
    /// The target as configured: the failure counters' `peer`.
    peer: String,
}

impl NamedClient {
    pub(super) fn new(target: Arc<ResolvedTarget>, settings: &ClientSettings) -> Self {
        let (host, port) = target.host_port();
        let peer = target.to_string();
        Self {
            client: build(settings, Some(target.clone())),
            target,
            base: format!("http://{host}:{port}"),
            peer,
        }
    }
}

#[async_trait]
impl HttpClient for NamedClient {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        if self.target.resolve_all().await.is_none() {
            crate::failures::record(&self.peer, crate::failures::FailureCause::Dns);
            return Err(HttpError::dns(&self.peer));
        }
        let url = format!("{}{}", self.base, req.path);
        send(&self.client, &self.peer, &url, req).await
    }
}
