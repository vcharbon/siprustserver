# A forwarding target is a name until the send

**Status:** accepted

**Owner:** `crates/sip-proxy/src/addr.rs` (`ProxyAddr`), `src/resolver.rs`,
`src/resolver/lookup.rs`.

## Context

A target the proxy forwards to is read off a Route, a Request-URI, or the forward-all
strategy's `target=host:port` cookie. Any of them may hold a host name. Resolving where the
message is parsed puts a DNS lookup on the receive loop; resolving later needs the name kept.
In a cluster the system resolver is configured with a high `ndots` (Kubernetes ships 5), so a
dotted name such as a Service FQDN walks every search domain before it is tried as given.

## Decision

### §1 A string host and a port

`ProxyAddr` holds a string host and a `u16` port, not a `SocketAddr`. The target resolves only
at the send: an IP literal is read in place, a name goes to the bounded named forwarder
(ADR-0046 §8). A `SocketAddr` field was rejected: it forces a lookup on the parse path and
loses the name the message carries.

### §2 A dotted name is queried absolute first

A dotted name not yet absolute is queried with a trailing root dot first, which the system
resolver answers in one lookup, then as given, so a short name that needs search expansion
still resolves. A single-label or already absolute name is queried once, as given. Changing
the pod's `resolv.conf` was rejected: the proxy does not own it.

## Consequences

- Every send to a name pays a cache read; a cold name costs one bounded lookup (ADR-0046 §8).
- The search-domain walk costs a lookup only for a name the absolute query misses.
