# A request sent off the call's turn is answered by its deadline

**Status:** accepted (2026-10-01); **superseded in part by
[ADR-0043](0043-outbound-backend-client-policy.md)** (the fold of an unanswered
`/call/failure` consult is the stack's own bare `503`). Amended (2026-10-29):
decision 5, a node serving the call from its record sends every awaited
request again; the deadline bounds the answer of the request as last sent.

## Context

A service sends two kinds of request off the call's turn and waits, in a
state of its own machine, for the answer to come back as an internal event:
an adaptation HTTP request (`RuleAction::ServiceHttpRequest` →
`service-http-result`) and a replacement of the call's admission set
(`RuleAction::ReplaceAdmissionSet` → `limiter-admit-result`, ADR-0040). The
task awaiting the answer is spawned on the node that sent the request. On a
healthy node it always answers: an HTTP request is bounded by its budget, and
the worker bounds every admit just past the admit budget.
When that node dies the task dies with it, and the call taken over by the
backup or reclaimed by the rebooted primary (ADR-0014) waits for an answer
that never comes: the service is stranded in its waiting state until the call
ends some other way. The core's own `/call/failure` consult
(`RuleAction::FailureAsyncHttp` → `call-failure-result`) is sent and lost
the same way, and leaves a caller in setup waiting until the call's setup
deadline.

## Decision

1. **The turn that sends the request arms the request's answer deadline** in
   the call's timer ledger, which is replicated with the call and re-armed on
   every materialisation: `ServiceHttpAnswer { correlation_id }` or
   `ServiceAdmitAnswer { correlation_id, change }`. It fires at the request's
   budget plus a fixed margin of 2 s: the HTTP request's `timeout_ms`, else the
   adaptation port's `default_timeout`; the admit's `CallLimiter::admit_budget`
   (the wire client's admit timeout; every limiter states one, a wrapper the
   one of what it wraps). The worker bounds every admit 100 ms past that
   budget, below its circuit breaker, so the limiter's own timeout fires first
   and is counted (the breaker reads it). An admit the limiter leaves
   unanswered is ended at that instant and counted a timeout, so a stalled
   limiter opens the breaker, when it has one. On a healthy node the answer is
   therefore always in before the deadline, and reaches the call: the router
   admits it past every per-call bound (ADR-0037 item 9).
2. **The ledger holds a deadline exactly while its answer is awaited.** Before
   the rules read an event, the router screens it against the ledger: the
   answer of a request whose deadline is there takes it off (and cancels it)
   and reaches the rules; an answer whose deadline is not there reaches no
   rule, though an admit's report is still applied to the call's limiter
   state, which it states truly. An admit report under another limiter key
   (an earlier call under the same `call_ref`, whose correlation ids may
   repeat this call's) reaches no rule and leaves the call's deadline
   awaited.
3. **The deadline's expiry is the answer with nothing usable**: an HTTP
   request's `outcome:"error"` result stating `"error":"no_answer"`, an
   admit's `unavailable` report (it may have landed; the next admit or refresh
   answer states what the limiter holds, ADR-0040). The service reads it as it
   reads a timeout. An expiry whose deadline is no longer in the ledger (its
   answer came first) reaches no rule.
4. **A `/call/failure` consult is answered by its deadline too**:
   `FailureAnswer { change, unanswered }`, named by the first change number
   the consult reserved, which its fold states as `consult_change`. Its
   budget is the whole failover chain the consult may run: `FAILURE_CHAIN`
   (`MAX_LIMITER_FAILOVER + 1` = 6) rounds of the decision engine's
   per-consult deadline (`call_control_timeout_ms`) plus the route's admit
   (the admit budget, the worker bounding it 100 ms past, item 1), so
   6 × (5 s + admit budget + 0.1 s) + 2 s under the default decision
   deadline. The expiry is the fold an unanswered consult resolves to:
   `terminate`, stack-authored, relaying the failed final the consult's seed
   stated (or letting a timeout stand) and ending the call. A late fold's
   admit report is still applied (2.). With the decision deadline off
   (`call_control_timeout_ms <= 0`) the consult is unbounded: no deadline is
   armed and its fold names none. The turn that arms the deadline names it on
   the consult it sends, so the fold states exactly the deadline armed.

5. **A node serving the call from its record sends every awaited request
   again.** The turn that sends a request also writes its image into the
   record (`Call::awaited`), so under the `applied` sync level the backup holds
   it before it leaves (ADR-0055 §3); its answer or its deadline's expiry takes
   it off. Every materialisation that serves the call — a takeover, a reclaim,
   the arming of a parked reclaim — queues one turn of the call's own that
   sends each awaited request again and arms its deadline afresh from that
   turn, the deadline its sender armed retired: an outside answer is not lost
   with the node that awaited it (user ruling). An admit the request carries
   is numbered above every change the call reserved before, so the limiter
   checks it net of the set the key holds and counts nothing twice (ADR-0040).
   The backend may see the request twice: delivery is at least once
   (ADR-0055 §1). A takeover copy that awaits an answer is not shed before
   its turn took it.

A correlation id names one request in flight: a service reusing one for a
second request while the first is awaited merges their deadlines.

## Consequences

- A service needs no watchdog of its own for a request's answer; every
  waiting state it has on one is bounded by the request's budget plus the
  margin, on whichever node serves the call.
- A primary read dead while it lives keeps the requests it awaits: its
  replication stream stays open, so the dead verdict takes nothing over
  (ADR-0047 D1), and no request is sent twice and no transfer is repeated. A copy that still sends a request again while
  its primary lives — an LB misroute's message, a record behind the wire under
  `off` or past the cap — acts on its own answer while the primary acts on its
  (ADR-0055 §11, `W-stale-resend`); the deadline still fires wherever the call
  is served, bounding the request as last sent.
- A release event's consult (`ReleaseAsyncHttp`, the subscribed maximum
  duration) has a deadline of its own (`ReleaseAnswer`), named by the change
  its turn reserved: the decision deadline plus the admit budget of the route
  it may answer, plus the margin. Its fold names it and, like a failure fold,
  reaches the rules only while the deadline is in the ledger: a second answer
  of the consult reaches none. A node serving the call from its record sends
  the consult again at once (decision 5); the deadline's expiry is the
  release event again, wherever the call is then served, so the consult is
  re-sent, and ends within its own decision deadline (unanswered: the local
  teardown). The consult is at least
  once: after a node loss, or an answer past its deadline, the decision layer
  may see the same release event twice; one answer applies — a fold no
  consult awaits writes no decision mark and moves no route onto the call,
  its admit report applied as an admit. With the decision deadline off
  (`call_control_timeout_ms` 0) the consult has no deadline: only a
  materialisation sends it again (decision 5).
- The core's REFER authorization needs no deadline; something already in the
  call's replicated timer list bounds it:
  - a REFER's authorization (`ReferAsyncHttp`): a node serving the call from
    its record sends it again (decision 5); its subscription's expiry
    (`ReferSubscriptionExpiry`) ends the wait and tells the referrer the
    transfer failed (NOTIFY `terminated`, 500).
- Tests: `b2bua/src/answer_deadline.rs` (arming, screening, the lost answers),
  `b2bua/src/router/answer_deadline_tests.rs` (the router's turn: a late
  answer reaches no rule, a late admit report still states the held set),
  `b2bua-harness/tests/it/service_admission_set.rs`
  (`a_stalled_service_admit_counts_as_a_limiter_failure`),
  `b2bua-harness/tests/it/failure_consult_bounds.rs` (a stalled failover
  admit lands its fold just past the admit budget; an unbounded consult's
  fold reaches the rules), `b2bua-harness/tests/it/initial_admit_bound.rs` (a
  stalled initial admit without a breaker fails open just past the admit
  budget), `b2bua/src/limiter/bounded.rs` (the bound),
  `b2bua/src/limiter/breaker.rs`
  (`an_admit_the_limiter_never_answers_counts_as_a_timeout`),
  `failover-harness/tests/it/service_answer_deadline.rs` (a node crashing with
  each kind of request in flight; the call reclaimed by the rebooted primary,
  taken over by the backup on a message or at the proxy's dead verdict, sends
  the request again and reads its outside answer, ending with one CDR and the
  limiter drained),
  `failover-harness/tests/it/consult_answer_deadline.rs` (a node crashing with
  each core consult in flight, the engine answering none: the consult sent
  again on reclaim and takeover, a `/call/failure` consult's 503 and the
  release consult's teardown at the re-sent consult's decision deadline; the
  REFER's authorization sent again and ended at the subscription's expiry),
  `b2bua-core/src/off_turn.rs` (the record of the awaited requests),
  `b2bua-harness/tests/it/release_consult_answer.rs` (a reroute whose admit
  runs long answers by the consult's deadline; the deadline's expiry
  re-sends the consult on the node serving the call; a release fold whose
  consult is no longer awaited reaches no rule and moves nothing of the
  call).
