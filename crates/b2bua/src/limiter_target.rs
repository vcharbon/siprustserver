//! [`LimiterTarget`] — where the limiter client sends its requests.
//!
//! A target is a socket address, or a `host:port` name resolved on the
//! request path: a name that does not resolve fails the request like a
//! transport error, inside the request's budget, so a limiter whose name
//! resolves late is reached once it does. The first address a name resolves
//! to is kept for the life of the client.

use std::net::SocketAddr;
use std::sync::OnceLock;

/// The limiter's address, or the name that resolves to it.
pub struct LimiterTarget {
    kind: Kind,
}

enum Kind {
    Addr(SocketAddr),
    Name { name: String, resolved: OnceLock<SocketAddr> },
}

impl LimiterTarget {
    /// A target at `addr`.
    pub fn addr(addr: SocketAddr) -> Self {
        Self { kind: Kind::Addr(addr) }
    }

    /// A target named `host:port`, resolved on the first request that needs
    /// it and on every request until it resolves.
    pub fn name(name: impl Into<String>) -> Self {
        Self { kind: Kind::Name { name: name.into(), resolved: OnceLock::new() } }
    }

    /// The address to send to; `None` while the name does not resolve.
    pub async fn resolve(&self) -> Option<SocketAddr> {
        match &self.kind {
            Kind::Addr(addr) => Some(*addr),
            Kind::Name { name, resolved } => {
                if let Some(addr) = resolved.get() {
                    return Some(*addr);
                }
                let addr = tokio::net::lookup_host(name.as_str()).await.ok()?.next()?;
                Some(*resolved.get_or_init(|| addr))
            }
        }
    }
}

impl std::fmt::Display for LimiterTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            Kind::Addr(addr) => addr.fmt(f),
            Kind::Name { name, .. } => f.write_str(name),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn an_address_is_its_own_resolution() {
        let addr: SocketAddr = "10.0.0.1:8080".parse().unwrap();
        assert_eq!(LimiterTarget::addr(addr).resolve().await, Some(addr));
    }

    #[tokio::test]
    async fn a_name_resolves_and_keeps_its_address() {
        let target = LimiterTarget::name("127.0.0.1:8080");
        let addr: SocketAddr = "127.0.0.1:8080".parse().unwrap();
        assert_eq!(target.resolve().await, Some(addr));
        assert_eq!(target.resolve().await, Some(addr));
        assert_eq!(target.to_string(), "127.0.0.1:8080");
    }

    #[tokio::test]
    async fn a_name_that_does_not_resolve_has_no_address() {
        // RFC 6761: `.invalid` never resolves.
        let target = LimiterTarget::name("limiter.invalid:8080");
        assert_eq!(target.resolve().await, None);
        assert_eq!(LimiterTarget::name("no-port").resolve().await, None);
    }
}
