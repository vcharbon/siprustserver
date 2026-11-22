# Appendix: Outbound backend client policy

The decision part: [ADR-0043](../0043-outbound-backend-client-policy.md).

## Decision

### 2. Name lookup through the target

The target is `ResolvedTarget`, plugged as
the pooled client's resolver. Lookups are single-flight and bounded; a lookup past
its bound frees its slot and runs on until it ends by itself (a blocking
resolver thread cannot be cancelled), and no new lookup starts while a cap
of them is stuck. Every address found is returned. The addresses are
**forgotten** when the breaker opens and when a probe fails; at each client
**rotation** they are **refreshed**: the first request past the period
starts a new lookup and is sent at once to the last good addresses, never
waiting for it; what the lookup finds replaces them, and when it finds
nothing they keep serving until the next rotation (serve stale). A request
resolves its target before the transport connects, under its own attempt
budget, so a lookup is bounded by the lookup bound or the attempt, never cut
by the connect timeout, and an attempt that ends with no address fails as a
name that does not resolve (`Dns`), never as a timeout. A client whose name has no address at boot opens its breaker once
that first lookup finds nothing (`BackendClient::trip_unless_resolved`).

### 5. Rotation

Once the rotation period has passed, the target's fresh
lookup starts and the current client keeps taking requests; a new client
takes over once that lookup is over (landed, past its bound, or refused by
the stuck cap), so its first connections go to what the lookup found and a
moved name is reached within one period. The old client finishes its
in-flight requests and closes its connections when the last of them drops
it.

### 6. Budgets

One attempt has its own budget, inside a total. The total is
the caller's (the decision deadline, `call_control_timeout_ms`); every
attempt is capped by the ceiling. A consult on a setup's turn is cut earlier when what is left of the
setup budget is shorter (ADR-0022 X1): the time the turn waited comes off its
last attempt.

### 7. Retry rule

`BackendClient::send_with_retry` makes one retry, on the same
client, after a connection-level failure (refused, unreachable, connect
timeout, reset, EOF, broken pipe) or a request timeout, when the total
budget leaves room for a whole attempt after the first one was due to end
(read from its deadline, not from the clock after it, so a total of two
attempts always holds the second). Never after an HTTP response of any status, never while the breaker
is open, never for a name that does not resolve or a request never sent.
A response is a whole one, status, headers and the body to its end: a body
that stalls is a request timeout and one the peer closes before its declared
end is an EOF, both retried, and neither is a response for the breaker (8).
A body past the size cap is not retried, and is a response for the breaker:
its head arrived. The backend may therefore serve a
retried request twice: retry suits a request that is safe to repeat.
The pool hands out the most recently returned idle connection first, so a
retry lands on a connection that just proved itself. `hyper-util`'s own
resend of a request never written on a reused connection stays. A caller
that must not retry (the limiter's admit, which fails open) uses one
attempt (`BackendClient::attempt`). A service's requests through the
adaptation port (`AdaptationHttpPort`) follow the rule, each attempt under
the port's attempt budget inside the request's own.

### 8. Breaker

`WindowBreaker`, one per backend, opens when at least
the configured number of requests were in flight during the last window
(sent inside it, or sent before it and unanswered when it began) and not
one HTTP response of any status arrived inside it, evaluated each time a
request ends without one. One dead peer among several never opens it (the
others answer); a total outage or a hung application does, whatever the
attempt budget against the window. A request never sent (no slot) is not
counted; one its caller gives up on stays counted but opens nothing. An
answer whose body the client refused past its cap (14) is a response: a
backend answering bodies past the cap is alive, and opening on it would
fail every request at once and gate the node's OPTIONS (11). While
open, a request fails at once, nothing sent, and a probe runs every period:
it forgets the addresses, looks the name up afresh, and asks the backend's
health path, under at most one period by default, on a client that keeps
no pool; any HTTP response closes the breaker, one refused past the cap
included. A probe that panics runs again the next period.

### 10. A decision backend failure is a bare 503

No `Reason` and no
`Retry-After` ride it, wherever the stack authors the final: the initial INVITE
(`CallDecisionError::Unavailable`, the `DeadlineDecisionEngine` expiry
included), the `/call/failure` consult (`failure_unavailable`: an engine
error or the answer's deadline, after a b-leg final or after a limiter
refusal alike; none of the failed peer's relayed headers), a setup whose
budget ran out, before its turn ran or anywhere in its failover chain
(ADR-0022 X1), a limiter refusal with no way to fail over (ADR-0022 X3),
and the REFER NOTIFY sipfrag (`SIP/2.0 503`, as if the
transfer target had answered 503). It is no decision: it marks nothing in
the decision log.

### 11. Backend gate on the OPTIONS self-health only

`/ready` and the replication membership ignore the gate, so a backend outage across the
cluster never empties the service.

### 12. Proxy fallback

A node not-ready on a backend still serves calls.

### 14. Per-connection ejection

A balancer in front of several backends behind one address (a Kubernetes
Service under kube-proxy) picks a backend per new TCP connection, and the
client never sees the backends: a pooled connection is its only handle on
one of them. The pool hands out the most recently returned connection
first and an HTTP answer is never retried (7), so a connection to a broken
backend keeps refusing the requests it carries until the client rotates.
Closing it makes the next request take another pooled connection or a new
one, which the balancer spreads again. Each client keeps a record of each
of its connections (`http_net::ejection::ConnectionRecord`, riding
hyper-util's `Connected::extra`) and closes exactly the one that
misbehaves:

- **A broken backend**: the first answer whose status is in
  `<P>_CONN_CLOSE_STATUSES` (500, 502, 504). A count would only multiply
  the refusals a broken backend costs.
- **A backend shedding load**: `<P>_CONN_SHED_CLOSE_AFTER` (3) answers in
  a row on one connection whose status is in `<P>_CONN_SHED_STATUSES` (503,
  429, 408); any other answer on it starts the run again, so one 503
  between two successes closes nothing.
- Both within `<P>_ERROR_CLOSES_PER_S` (20) closes a second per client: at
  a total outage every answer would close its connection, and each close
  leaves a socket in `TIME_WAIT` for 60 s. Past it the connection is pooled
  and counted (`http_request_connection_closes_budget_exhausted_total`).
- **A timeout**: a request dropped, or cut by the ceiling, before its answer
  was read whole abandons its connection, always, outside the budget: the
  answer may still come on it and must never be read as another request's.
  The close is a reset (`SO_LINGER` 0, no FIN first), so a timeout storm
  leaves no socket in `TIME_WAIT` against one address. The `reqwest` client
  this one replaced never reused a timed-out connection either; it closed it
  with a FIN.
- Latency alone never closes a connection: a backend's answer time depends
  on the services it calls in turn.

A connection the rules close is poisoned before the request yields again,
so the pool never hands it out; it closes once its answer is read. On a
multi-thread runtime hyper-util may put the connection back in its pool as
the answer resolves, before the poison, and a request on another thread may
take it in that window: a connection the rules close carries at most two
requests, its close counted once. The verdict and the poison come before
anything is counted, so the window is the answer's own poll. A refused
connect (a dead backend) stays a connect failure, retried under 7.

An answer body is read under `<P>_MAX_BODY_BYTES` (10 MiB unless a backend
states less): a declared length past it is refused before any of the body
is read; a body with no declared length (chunked, or read to the close) is
read up to the cap and cut there, partly read. Both fail `BodyTooLarge`, are never retried,
count as a response for the breaker (8), and close their connection. The
buffer grows by doubling and never past the cap. Counted as
`http_request_body_refused_total{cause}`.

Every close is counted by cause in `http_request_connection_closes_total`:
`status`, `shed`, `timeout`, `rotation` (its client retired) and `idle`
(past the idle timeout or the idle cap). A connection its peer ended (an
end of stream, or an answer with `Connection: close` or HTTP/1.0 without
keep-alive), that a failed exchange or a refused body ended, or that a
client keeping no pool (the probe) closed, is not a close of the client's.
Each 5xx is counted by what became of its connection
(`http_request_error_answers_total{connection}`). The simulated fabric's
pooled client applies the same rules and the same body cap.

The client sends only the headers its caller states, `Host` and the body's
framing aside: no `Accept` unless the caller sets one (the `reqwest` client before it sent
`Accept: */*`).

### Settings and their env keys

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
| `<P>_CONN_CLOSE_STATUSES` | statuses whose first answer closes its connection (14) | `500,502,504` |
| `<P>_CONN_SHED_STATUSES` | statuses whose answers in a row close their connection, disjoint from the above | `408,429,503` |
| `<P>_CONN_SHED_CLOSE_AFTER` | the run of shed answers that closes a connection | 3 |
| `<P>_ERROR_CLOSES_PER_S` | closes on answers per second and client, 0 never | 20 |
| `<P>_MAX_BODY_BYTES` | the largest answer body read, at least 1024 | 10485760 |
| `<P>_BREAKER_REQUESTS` | requests in the window, none answered, that open the breaker | 3 |
| `<P>_BREAKER_WINDOW_MS` | the breaker window | 2000 |
| `<P>_BREAKER_PROBE_MS` | probe period while open | 1000 |
| `<P>_BREAKER_PROBE_TIMEOUT_MS` | one probe request, after its lookup | the attempt budget, at most the probe period |
| `<P>_HEALTH_PATH` | the path the probe asks with `GET` | per backend (limiter: `/v1/health`) |
| `<P>_LOOKUP_TIMEOUT_MS` | one name lookup | 2000 |
| `<P>_LOOKUP_MAX_STUCK` | lookups past their bound before none starts | 4 |

At a high call rate the breaker's request threshold belongs well above 3: a
burst of a few lost requests should not open it. A set that does not read
refuses boot with one error naming every variable involved.

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

- A worker whose decision backend does not resolve at boot starts without
  waiting for the lookup; its breaker opens once that first lookup finds
  nothing (within the lookup bound), from then on it answers 503 at once, and
  it routes through the backend once the name resolves and a probe is
  answered.
- The limiter's breaker follows the same rule: a limiter answering every
  request `5xx` keeps it closed (each admit fails open on its own budget);
  only silence opens it. Refreshes and releases count as requests.
- The unanswered `/call/failure` consult relays none of the failed leg's
  final (ADR-0039's fold of an unanswered consult is this 503).
- Pinned by: `http-net` `tests/it/backend.rs` (retry, cap, rotation, breaker,
  lookup bound), `tests/it/backend_breaker.rs` (a hung burst, slot waits, a
  panicking probe, the boot lookup), `tests/it/classify.rs` (the loopback
  classifier, a hanging lookup on the real client), `tests/it/real.rs` (a
  body cut before its end, the size cap), `tests/it/real_backend.rs`
  (the retry's room, a cut answer retried and a moved name, on the real clock),
  `tests/it/real_closes.rs` and `tests/it/real_timeout.rs` (ejection on the
  real client behind a balancing front, on a multi-thread runtime too, the
  timeout's reset), `tests/it/real_peer_ends.rs` (the closes not counted),
  `real_backend.rs` (a body past the cap at the breaker),
  `tests/body_cap_alloc.rs` (the body cap at the allocator), `ejection.rs`
  (the rules and the budget); `http-net-testkit` `tests/it/simulated_balanced.rs`
  (the same rules and body cap on the simulated fabric),
  `resolved_target/tests.rs` (stuck lookups, serve stale); `b2bua-harness`
  `unanswered_failure_consult`, `decision_deadline`, `refer_consult_error`,
  `service_http` (the adaptation port's retry),
  `options_backend_gate`, `limiter_breaker`; `sip-proxy` `load_balancer`
  (the not-ready fallback and its two guards).
