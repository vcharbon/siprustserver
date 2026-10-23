//! [`AddressedClient`] — a backend client over any transport: it resolves its
//! target on each request and sends to the first address found through the
//! transport's [`request`](HttpTransport::request). The default of
//! [`HttpTransport::client`], so the simulated fabric and every decorator
//! serve a backend client unchanged.

use std::sync::Arc;

use async_trait::async_trait;

use super::{HttpClient, HttpError, HttpRequest, HttpResponse, HttpTransport};
use crate::resolved_target::ResolvedTarget;

/// A client sending to `target`'s address over `transport`.
pub struct AddressedClient<T: ?Sized> {
    transport: Arc<T>,
    target: Arc<ResolvedTarget>,
}

impl<T: HttpTransport + ?Sized> AddressedClient<T> {
    /// A client of `target` over `transport`.
    pub fn new(transport: Arc<T>, target: Arc<ResolvedTarget>) -> Self {
        Self { transport, target }
    }
}

#[async_trait]
impl<T: HttpTransport + ?Sized> HttpClient for AddressedClient<T> {
    async fn request(&self, req: HttpRequest) -> Result<HttpResponse, HttpError> {
        let Some(addr) = self.target.resolve().await else {
            return Err(HttpError::dns(&*self.target));
        };
        self.transport.request(addr, req).await
    }
}
