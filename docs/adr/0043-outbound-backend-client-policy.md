# Outbound backend client policy

**Status:** accepted (2026-10-11). Amended with ADR-0050 (§11, §12) and ADR-0022 X1 (§6, §10).

**Owner:** `crates/http-net/src/backend/` (the backend client, its breaker, probe, rotation,
in-flight slots, retry rule and env grammar), `crates/http-net/src/resolved_target.rs` and
`resolver.rs` (name lookup), `crates/http-net/src/settings.rs` and `transport/real_client.rs`
(the `reqwest` client settings); `crates/b2bua-runner-kit/src/backend_url.rs` (a backend's
URL in the env); the SIP side is `crates/b2bua/src/failure_unavailable.rs`,
`decision/limiter_failover.rs`, `rules/refer_transfer/machine/authorizing.rs`,
`repl/backend_gate.rs` and `limiter/breaker.rs`;
`crates/sip-proxy/src/strategies/load_balancer.rs` for the new-dialog fallback.

The long form of each rule, the settings table, the other outbound clients and the tests:
[appendix](appendix/0043-outbound-backend-client-policy.md).

## Context

A worker calls HTTP backends on its request path: a decision backend (the new-call, failure
and REFER consults), the call limiter, and any service's own backend. A stock client has no
connect timeout and no ceiling on a request, keeps one connection set for the life of the
process so a load balancer never sees new connections, and resolves a name once at boot with a
lookup that can block every later one. Without a policy, a backend failure reaches the caller
as whatever final the engine chooses, and a cluster-wide outage leaves every worker healthy.

## Decision

1. **One client per backend** (`BackendClient`), over `HttpTransport::client`: stock
   `reqwest`, no connection pool of our own. The URL carries the configured name, so `Host` is
   the name.
2. **Name lookup through the target** (`ResolvedTarget`, plugged as `reqwest`'s resolver).
   Lookups are single-flight and bounded, and none starts while a cap of them is stuck. The
   addresses are forgotten when the breaker opens and when a probe fails, and refreshed at
   each rotation, serving stale meanwhile. An attempt that ends with no address fails as `Dns`,
   never as a timeout. A client whose name has no address at boot opens its breaker once that
   first lookup finds nothing. Detail: [appendix §2](appendix/0043-outbound-backend-client-policy.md#2-name-lookup-through-the-target).
3. **Transport settings** (`ClientSettings`): connect timeout, an idle timeout below a typical
   backend keep-alive, TCP keepalive and `TCP_USER_TIMEOUT`, a ceiling on any one request, so
   no request is ever unbounded.
4. **In-flight cap**: a semaphore on requests in flight, with `pool_max_idle_per_host` at the
   same value, caps the TCP connections. A request waits for a slot inside its own attempt
   budget, and one whose budget runs out waiting is never sent and never reaches the breaker.
   Counted as `http_request_slot_*`.
5. **Rotation**: past the rotation period the target's fresh lookup starts while the current
   client keeps serving. A new client takes over once that lookup is over, so a moved name is
   reached within one period. Detail: [appendix §5](appendix/0043-outbound-backend-client-policy.md#5-rotation).
6. **Budgets**: one attempt has its own budget, inside a total. The total is the caller's (the
   decision deadline, `call_control_timeout_ms`), and the ceiling caps every attempt. A consult
   on a setup's turn is cut earlier when less of the setup budget is left (ADR-0022 X1).
   Detail: [appendix §6](appendix/0043-outbound-backend-client-policy.md#6-budgets).
7. **Retry rule** (`BackendClient::send_with_retry`): one retry, on the same client, after a
   connection-level failure or a request timeout, when the total budget leaves room for a whole
   attempt. Never after an HTTP response of any status, never while the breaker is open, never
   for a name that does not resolve or a request never sent. A caller that must not retry uses
   `BackendClient::attempt`. The adaptation port (`AdaptationHttpPort`) follows the rule.
   Detail: [appendix §7](appendix/0043-outbound-backend-client-policy.md#7-retry-rule).
8. **Breaker** (`WindowBreaker`, one per backend): it opens when at least the configured number
   of requests were in flight during the last window and not one HTTP response of any status
   arrived inside it. While open, a request fails at once, and a probe runs every period on a
   fresh lookup against the backend's health path. Any HTTP response closes it. Detail:
   [appendix §8](appendix/0043-outbound-backend-client-policy.md#8-breaker).
9. **Policy while open is the caller's.** The limiter lets the call through uncounted and holds
   its release queue and refresh batch until the close (ADR-0040). A decision backend fails the
   call (10).
10. **A decision backend failure is a bare 503**, no `Reason`, no `Retry-After`, wherever the
    stack authors the final: the initial INVITE, the `/call/failure` consult, a setup whose
    budget ran out, a limiter refusal with no way to fail over, and the REFER NOTIFY sipfrag
    (`SIP/2.0 503`). It is no decision and marks nothing in the decision log. Detail:
    [appendix §10](appendix/0043-outbound-backend-client-policy.md#10-a-decision-backend-failure-is-a-bare-503).
11. **Backend gate on the OPTIONS self-health only** (`Readiness::gate_options_on`): while a
    gating backend's breaker is open, a `Ready` node answers OPTIONS `503` not-ready, and `200`
    again once it closes; never latched. `/ready` and the replication membership ignore it. The
    limiter never gates. Detail: [appendix §11](appendix/0043-outbound-backend-client-policy.md#11-backend-gate-on-the-options-self-health-only).
12. **Proxy fallback**: `select_for_new_dialog` takes the not-ready workers only when no worker
    is alive, whatever the cause. In-dialog routing is unchanged. Detail:
    [appendix §12](appendix/0043-outbound-backend-client-policy.md#12-proxy-fallback).
13. **No blocking lookup on the router task**: per-peer failure attribution reads the outbound
    proxy's kept addresses and starts its lookup off the task. A named hop other than the proxy
    is not attributed.

### Settings and their env keys

Read at boot under the backend's prefix (`BackendConfig::from_lookup`; the limiter's prefix is
`LIMITER`). Unset takes the default. A value that is not a positive integer, or a health path
not starting with `/`, refuses boot. At a high call rate the breaker's request threshold
belongs well above 3. Keys and defaults: [appendix, settings](appendix/0043-outbound-backend-client-policy.md#settings-and-their-env-keys).

### Other outbound clients (checked, unchanged)

CDR export over AMQP and OTLP trace export bound every wait of their own. Detail:
[appendix, other clients](appendix/0043-outbound-backend-client-policy.md#other-outbound-clients-checked-unchanged).

## Consequences

- A worker whose decision backend does not resolve at boot starts without waiting, answers 503
  at once once its breaker opens, and routes through the backend once the name resolves and a
  probe is answered.
- Only silence opens the limiter's breaker: a limiter answering every request `5xx` keeps it
  closed. Refreshes and releases count as requests.
- An unanswered `/call/failure` consult never relays the failed leg's final (ADR-0039).

Tests: [appendix, consequences](appendix/0043-outbound-backend-client-policy.md#consequences).
