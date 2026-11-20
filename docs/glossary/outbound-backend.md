# Outbound backend vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Outbound backend vocabulary".

A worker's HTTP backends on the request path (a decision backend, the call
limiter) and the client policy they share. See
[ADR-0043](../../docs/adr/0043-outbound-backend-client-policy.md).

**Backend client** (`http_net::BackendClient`):
One backend's client: its own `reqwest` client (rotated on a fixed period),
its name looked up through a **resolved target**, a cap on requests in
flight, a **backend breaker**, and the generic retry rule. Every value is an
env key under the backend's prefix (`<P>_…`).
_Avoid_: "connection pool" for it (the pool is `reqwest`'s own).

**Resolved target** (`http_net::ResolvedTarget`):
A backend's socket address, or its `host:port` name looked up on the request
path: single-flight, bounded, every address kept. **Forget** drops the
addresses (breaker open, failed probe); **refresh** looks the name up again
and **serves stale** when that lookup finds nothing (client rotation).

**Backend breaker**:
Opens when its window saw the configured number of requests in flight and no HTTP
response of any status; a probe of the backend's health path closes it on any
response. What an open breaker means for a call (fail it, let it through
uncounted) is the caller's **policy**.
_Avoid_: "circuit open" for a backend answering errors (an answered `5xx` keeps
it closed).

**Decision backend failure**:
No usable answer from the decision backend (timeout, transport error, breaker
open, a name that does not resolve, an unreadable answer). The stack answers a
bare `503` (no `Reason`, no `Retry-After`) on the initial INVITE and the
`/call/failure` consult, and `SIP/2.0 503` in a REFER sipfrag. A reject inside
an answer is a decision, not a failure.

**Backend gate** (OPTIONS):
A backend breaker registered on a node's acceptance of new calls: open, a `Ready`
node answers OPTIONS `200` with `X-Readiness: new=refuse; cause=backend` and keeps
serving its dialogs; never latched, never read by `/ready`.
