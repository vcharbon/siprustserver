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

use super::{HttpClient, HttpError, HttpRequest, HttpResponse};
use crate::resolved_target::ResolvedTarget;
use crate::settings::ClientSettings;

/// A pooled `http://` client under `settings`, its name lookups going
/// through `target` when one is given.
pub(super) fn build(
    settings: &ClientSettings,
    target: Option<Arc<ResolvedTarget>>,
) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
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
/// cause and returned classified.
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
    let method = reqwest::Method::from_bytes(req.method.as_bytes()).map_err(|e| {
        HttpError::new(peer, crate::failures::FailureCause::Other, format!("bad method: {e}"))
    })?;
    let mut builder = client.request(method, url);
    for (name, value) in &req.headers {
        builder = builder.header(name.as_str(), value.as_str());
    }
    let resp = builder.body(req.body).send().await.map_err(failed)?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), v.to_str().unwrap_or("").to_string()))
        .collect();
    let body = resp.bytes().await.map_err(failed)?.to_vec();
    Ok(HttpResponse { status, headers, body })
}

/// One backend's `reqwest` client: its own pool, its URL naming the target.
pub(super) struct NamedClient {
    client: reqwest::Client,
    /// `http://<target>`, the authority as configured.
    base: String,
    /// The target as configured: the failure counters' `peer`.
    peer: String,
}

impl NamedClient {
    pub(super) fn new(target: Arc<ResolvedTarget>, settings: &ClientSettings) -> Self {
        let (host, port) = target.host_port();
        let peer = target.to_string();
        Self { client: build(settings, Some(target)), base: format!("http://{host}:{port}"), peer }
    }
}

#[async_trait]
impl HttpClient for NamedClient {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let url = format!("{}{}", self.base, req.path);
        send(&self.client, &self.peer, &url, req).await
    }
}
