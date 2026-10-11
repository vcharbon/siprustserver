# Outbound backend client policy

**Status:** accepted (2026-10-11)

**Owner:** `crates/http-net/src/backend/` (the backend client, its breaker,
probe, rotation, in-flight slots, retry rule and env grammar),
`crates/http-net/src/resolved_target.rs` and `resolver.rs` (name lookup),
`crates/http-net/src/settings.rs` and `transport/real_client.rs` (the
`reqwest` client settings); `crates/b2bua-runner-kit/src/backend_url.rs` (a backend's
URL in the env); the SIP side is `crates/b2bua/src/failure_unavailable.rs`,
`decision/limiter_failover.rs`, `rules/refer_transfer/machine/authorizing.rs`,
`repl/backend_gate.rs` and `limiter/breaker.rs`;
`crates/sip-proxy/src/strategies/load_balancer.rs` for the new-dialog fallback.

## Context

A worker calls HTTP backends on its request path: a decision backend (the
new-call, failure and REFER consults), the call limiter, and any service's own
backend. The transport was a bare `reqwest` client: no connect timeout, no
ceiling on a request, a 90 s idle timeout, no limit on connections, and one
client for the life of the process, so a load balancer in front of a backend
never saw new connections under steady traffic. A backend name was resolved
once at boot with a blocking lookup, and a lookup that never returned blocked
every later one. A decision backend failure reached the caller as whatever
final the engine chose, and a backend outage across the cluster left every
worker reporting itself healthy.

## Decision

1. **One client per backend** (`BackendClient`), over the transport seam
   (`HttpTransport::client`): stock `reqwest`, no connection pool of our own.
   The URL carries the backend's name as configured, so `Host` is the name.
2. **Name lookup through the target** (`ResolvedTarget`, plugged as
   `reqwest`'s resolver). Lookups are single-flight and bounded; a lookup past
   its bound frees its slot and runs on until it ends by itself (a blocking
   resolver thread cannot be cancelled), and no new lookup starts while a cap
   of them is stuck. Every address found is returned. The addresses are
   **forgotten** when the breaker opens and when a probe fails; at each client
   **rotation** they are **refreshed**: the next request waits for a new
   lookup and, when it finds nothing, the last good addresses keep serving
   until the next rotation (serve stale).
3. **Transport settings** (`ClientSettings`): connect timeout, an idle timeout
   below a typical backend keep-alive, TCP keepalive and `TCP_USER_TIMEOUT`, a
   ceiling on any one request, so no request is ever unbounded.
4. **In-flight cap**: a semaphore on requests in flight, with
   `pool_max_idle_per_host` at the same value, caps the TCP connections. A
   request waits for a slot inside its own attempt budget; a request whose
   budget runs out waiting is never sent, and the wait never reaches the
   breaker. Waits, their time and the requests that never got a slot are
   counted (`http_client_slot_*`).
5. **Rotation**: a new client takes new requests once the rotation period has
   passed; the old one finishes its in-flight requests and closes its
   connections when the last of them drops it.
6. **Budgets**: one attempt has its own budget, inside a total. The total is
   the caller's (the decision deadline, `call_control_timeout_ms`), or the
   ceiling when the caller has none (`BackendClient::budgets` with a total
   `<= 0`).
7. **Retry rule** (`BackendClient::send_with_retry`): one retry, on the same
   client, after a connection-level failure (refused, unreachable, connect
   timeout, reset, EOF, broken pipe) or a request timeout, when the total
   budget has time left; the retry gets what is left, at most one attempt's
   budget. Never after an HTTP response of any status, never while the breaker
   is open, never for a name that does not resolve or a request never sent.
   The pool hands out the most recently returned idle connection first, so a
   retry lands on a connection that just proved itself. `hyper-util`'s own
   resend of a request never written on a reused connection stays. A caller
   that must not retry (the limiter's admit, which fails open) uses one
   attempt (`BackendClient::attempt`). A service's requests through the
   adaptation port (`AdaptationHttpPort`) follow the rule, each attempt under
   the port's attempt budget inside the request's own.
8. **Breaker** (`WindowBreaker`, one per backend): it opens when the last
   window saw at least the configured number of requests sent and not one
   HTTP response of any status, evaluated each time a request ends without
   one. One dead peer among several never opens it (the others answer); a
   total outage or a hung application does. While open, a request fails at
   once, nothing sent, and a probe runs every period: it forgets the
   addresses, looks the name up afresh, and asks the backend's health path on
   a client that keeps no pool; any HTTP response closes the breaker.
9. **Policy while open is the caller's.** The limiter lets the call through
   uncounted and holds its release queue and refresh batch until the close
   (ADR-0040); a decision backend fails the call (10).
10. **A decision backend failure is a bare 503** — no `Reason`, no
    `Retry-After` — wherever the stack authors the final: the initial INVITE
    (`CallDecisionError::Unavailable`, the `DeadlineDecisionEngine` expiry
    included), the `/call/failure` consult (`failure_unavailable`: an engine
    error or the answer's deadline, after a b-leg final or after a limiter
    refusal alike; none of the failed peer's relayed headers), and the REFER
    NOTIFY sipfrag (`SIP/2.0 503`, as if the transfer target had answered
    503). It is no decision: it marks nothing in the decision log.
11. **Backend gate on the OPTIONS self-health only**
    (`Readiness::gate_options_on`): while a gating backend's breaker is open,
    a `Ready` node answers OPTIONS `503` not-ready, and `200` again once it
    closes; never latched. `/ready` and the replication membership ignore it,
    so a backend outage across the cluster never empties the service. The
    limiter never gates.
12. **Proxy fallback**: `select_for_new_dialog` takes the not-ready workers
    only when no worker is alive, whatever the cause: a node not-ready on a
    backend still serves calls. In-dialog routing is unchanged.
13. **No blocking lookup on the router task**: per-peer failure attribution
    reads the outbound proxy's kept addresses and starts its lookup off the
    task; a named hop other than the proxy is not attributed.

### Settings and their env keys

Read at boot under the backend's prefix (`BackendConfig::from_lookup`; the
limiter's prefix is `LIMITER`). Unset takes the default; a value that is not a
positive integer (or a health path not starting with `/`) refuses boot.

| key | meaning | default |
|---|---|---|
| `<P>_TIMEOUT_MS` | one attempt's budget, its slot wait included | per backend (limiter: 150) |
| `<P>_CONNECT_TIMEOUT_MS` | one TCP connect | 1000 |
| `<P>_POOL_IDLE_MS` | idle pooled connection closed past it | 4000 |
| `<P>_KEEPALIVE_IDLE_MS` / `_INTERVAL_MS` / `_RETRIES` | TCP keepalive | 5000 / 2000 / 3 |
| `<P>_USER_TIMEOUT_MS` | `TCP_USER_TIMEOUT` | 10000 |
| `<P>_CEILING_MS` | any one request | 5000 |
| `<P>_MAX_IN_FLIGHT` | requests in flight, idle connections kept | per backend (limiter: 64, default 256) |
| `<P>_ROTATION_MS` | client rotation period | 60000 |
| `<P>_BREAKER_REQUESTS` | requests in the window, none answered, that open the breaker | 3 |
| `<P>_BREAKER_WINDOW_MS` | the breaker window | 2000 |
| `<P>_BREAKER_PROBE_MS` | probe period while open | 1000 |
| `<P>_BREAKER_PROBE_TIMEOUT_MS` | one probe request, after its lookup | the attempt budget |
| `<P>_HEALTH_PATH` | the path the probe asks with `GET` | per backend (limiter: `/v1/health`) |
| `<P>_LOOKUP_TIMEOUT_MS` | one name lookup | 2000 |
| `<P>_LOOKUP_MAX_STUCK` | lookups past their bound before none starts | 4 |

At a high call rate the breaker's request threshold belongs well above 3: a
burst of a few lost requests should not open it.

### Other outbound clients (checked, unchanged)

- **CDR export over AMQP** (`b2bua-runner-kit/src/cdr_rabbitmq`): every wait
  is bounded — the connect by its own timeout around the session open (the
  socket connect under a deadline), each publish and confirm by theirs, the
  broker connection by its heartbeat — and reconnects are spaced by a
  backoff. The broker name is looked up with a blocking `getaddrinfo` on a
  blocking-pool thread the AMQP client spawns: a hanging lookup holds that
  thread until the system resolver gives up, never the caller.
- **OTLP trace export** (`observe/src/otlp.rs`): the batch exporter runs on
  its own thread with a bounded queue, and each export is bounded by the
  exporter's timeout (`OTEL_EXPORTER_OTLP_TIMEOUT`, 10 s by default), its
  lookup included.

## Consequences

- A worker whose decision backend does not resolve at boot starts, answers
  503 at once while its breaker is open, and routes through the backend once
  the name resolves and a probe is answered.
- The limiter's breaker follows the same rule: a limiter answering every
  request `5xx` keeps it closed (each admit fails open on its own budget);
  only silence opens it. Refreshes and releases count as requests.
- The unanswered `/call/failure` consult no longer relays the failed leg's
  final (ADR-0039's fold of an unanswered consult is this 503).
- Pinned by: `http-net` `tests/it/backend.rs` (retry, cap, rotation, breaker,
  lookup bound), `tests/it/classify.rs` (the loopback classifier),
  `resolved_target/tests.rs` (stuck lookups, serve stale); `b2bua-harness`
  `unanswered_failure_consult`, `decision_deadline`, `refer_consult_error`,
  `service_http` (the adaptation port's retry),
  `options_backend_gate`, `limiter_breaker`; `sip-proxy` `load_balancer`
  (the not-ready fallback and its two guards).
