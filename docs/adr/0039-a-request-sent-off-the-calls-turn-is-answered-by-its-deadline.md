# A request sent off the call's turn is answered by its deadline

**Status:** accepted (2026-10-01); **superseded in part by
[ADR-0043](0043-outbound-backend-client-policy.md)** (the fold of an unanswered
`/call/failure` consult is the stack's own bare `503`). Amended (decision 5).

The long form of each decision, the release and REFER consults, and the tests:
[appendix](appendix/0039-a-request-sent-off-the-calls-turn-is-answered-by-its-deadline.md).

## Context

A service sends two kinds of request off the call's turn and waits, in a state of its own
machine, for the answer as an internal event: an adaptation HTTP request
(`RuleAction::ServiceHttpRequest` → `service-http-result`) and a replacement of the call's
admission set (`RuleAction::ReplaceAdmissionSet` → `limiter-admit-result`, ADR-0040). The task
awaiting the answer runs on the node that sent the request. When that node dies the task dies
with it, and the call taken over or reclaimed (ADR-0014) waits for an answer that never comes.
The core's `/call/failure` consult (`RuleAction::FailureAsyncHttp` → `call-failure-result`) is
lost the same way and leaves a caller in setup waiting until the setup deadline.

## Decision

1. **The turn that sends the request arms the request's answer deadline** in the call's
   replicated timer ledger, re-armed on every materialisation: `ServiceHttpAnswer
   { correlation_id }` or `ServiceAdmitAnswer { correlation_id, change }`. It fires at the
   request's budget plus a fixed 2 s margin: the HTTP request's `timeout_ms`, else the
   adaptation port's `default_timeout`; for an admit, `CallLimiter::admit_budget`. The worker
   bounds every admit 100 ms past that budget, below its circuit breaker. Detail:
   [appendix §1](appendix/0039-a-request-sent-off-the-calls-turn-is-answered-by-its-deadline.md#1-the-turn-that-sends-the-request-arms-the-requests-answer-deadline).
2. **The ledger holds a deadline exactly while its answer is awaited.** The router screens each
   event against the ledger before the rules read it. An answer whose deadline is there takes
   it off and reaches the rules. An answer whose deadline is not there reaches no rule, though
   an admit's report is still applied to the call's limiter state. Detail:
   [appendix §2](appendix/0039-a-request-sent-off-the-calls-turn-is-answered-by-its-deadline.md#2-the-ledger-holds-a-deadline-exactly-while-its-answer-is-awaited).
3. **The deadline's expiry is the answer with nothing usable**: an HTTP request's
   `outcome:"error"` result stating `"error":"no_answer"`, an admit's `unavailable` report. The
   service reads it as it reads a timeout. An expiry whose deadline is gone reaches no rule.
4. **A `/call/failure` consult is answered by its deadline too**: `FailureAnswer { change,
   unanswered }`, named by the first change number the consult reserved, which its fold states
   as `consult_change`. Its budget is the whole failover chain: `FAILURE_CHAIN` rounds of
   `call_control_timeout_ms` plus the route's admit, plus the margin. The expiry is the fold an
   unanswered consult resolves to, the stack's bare 503 (ADR-0043 §10), ending the call.
   Detail: [appendix §4](appendix/0039-a-request-sent-off-the-calls-turn-is-answered-by-its-deadline.md#4-a-callfailure-consult-is-answered-by-its-deadline-too).
5. **A node serving an `Active` call from its record sends every awaited request again.** The
   sending turn writes the request's image into the record (`Call::awaited`). Every
   materialisation that serves the call queues one turn that sends each awaited request again
   and arms its deadline afresh. A request is made for its answer alone, never for its effect
   at the service, so an ending call sends nothing again. Detail:
   [appendix §5](appendix/0039-a-request-sent-off-the-calls-turn-is-answered-by-its-deadline.md#5-a-node-serving-an-active-call-from-its-record-sends-every-awaited-request-again).

A correlation id names one request in flight: a service reusing one for a second request while
the first is awaited merges their deadlines.

## Consequences

- A service needs no watchdog of its own for a request's answer: every waiting state on one is
  bounded by the request's budget plus the margin, on whichever node serves the call.
- A primary read dead while it lives keeps the requests it awaits (ADR-0047 D1).
- A release event's consult (`ReleaseAsyncHttp`) has its own deadline, `ReleaseAnswer`.
- The core's REFER authorization needs no deadline: its subscription's expiry
  (`ReferSubscriptionExpiry`) bounds it.

Detail and tests: [appendix, consequences](appendix/0039-a-request-sent-off-the-calls-turn-is-answered-by-its-deadline.md#consequences).
