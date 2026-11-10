# Appendix: The initial-INVITE final-response guarantee (accept-and-forward, or 503 within the decision deadline)

The decision part: [ADR-0022](../0022-initial-invite-final-response-guarantee.md).

## Context: the three gaps

Without X1 and X2, three independent gaps break the guarantee.

1. **No deadline at the decision seam.** sip-txn auto-sends `100 Trying` the instant an
   INVITE server transaction is born (`layer.rs`, `handle_inbound_request`), before the
   router or the decision engine run. An initial-INVITE handler that `await`s
   `decision.new_call()` bare (`initial_invite.rs`) parks the per-call worker behind the
   caller's 100 for as long as a hung or slow call-control endpoint takes, including any
   third-party `CallDecisionEngine` that forgot its own timeout. The TS
   `HttpReferenceAdapter` bounds it with `callControlNewCallTimeoutMs = 5000`.
2. **Every backstop behind the seam is silent toward the caller.** A call reaches
   `→ terminated` with its a-leg INVITE unanswered on the reaper's `reaper-stale`,
   `reaper-fatal-error` and strike-2 discharge paths (`defaults.rs::reap_force_terminal`
   deliberately emits no wire messages), on a handler panic during INVITE handling, and when
   the sip-txn safety sweep `delete_txn`s an unanswered server transaction at ~193 s
   (`layer.rs::sweep`). On each the caller hears 100 and then nothing.
3. **Status-code drift.** Decision-error and overload rejects use `503`, and the requirement
   was stated informally as "500". X3 settles on 503 Service Unavailable: both the Rust port
   and the TS reference emit it, and it is the correct code for a transient server-side
   inability to handle the call.

## Decision X1: a core-level decision deadline (`DeadlineDecisionEngine`)

Each wrapped call rides `tokio::time::timeout`, deterministic under `start_paused`. On expiry
it returns `CallDecisionError::Unavailable`, which every call site maps to a final response.
The expired inner future is dropped, which cancels an HTTP adapter's in-flight request like
the TS `Effect.timeoutOrElse`. The default of 5000 ms matches the TS `CALL_CONTROL_*_TIMEOUT_MS`. No consult runs unbounded
while it holds its call's lock.

**Scope: `new_call` + `call_failure`, deliberately NOT `call_refer`.** These two

are the decision calls that can block a caller waiting behind the INVITE's
auto-100: `new_call` for the initial route, and `call_failure` for the
limiter-reject / no-answer failover that reroutes *toward a still-pending INVITE
final*. `call_refer` is different in kind — the REFER already received its
`202 Accepted`, so a hanging refer authorization strands no waiting INVITE; it is
bounded instead by the dedicated `refer_subscription_expiry_sec` (60 s) and
`refer_overall_safety_sec` (120 s) timers (`refer_reject.rs::refer_http_timeout`).
This is a **documented divergence** from the TS `callControlReferTimeoutMs`: the
Rust port bounds the REFER lifecycle with those subscription timers rather than a
decision deadline, so `call_refer` passes straight through the wrapper.

The setup budget bounds the whole setup on an initial INVITE's turn, not each round trip: a
caller behind the 100 waits for all of it together. The transaction absorbs a retransmission
(RFC 3261 §17.2.3). A copy on a new branch (§8.2.2.2) opened a transaction of its own and
heard its own 100. It is no new call, but its turn stops waiting at its own budget and
answers its 482. A turn that stops waiting at the deadline (`Runnable::due_by`) and finds its
budget spent has its call born only to write its record, and the INVITE is counted once. A
setup its caller CANCELed first gets no second final. The budget is no admission rung, since
a caller left waiting serves nobody. `DeadlineDecisionEngine` is the one bound on a round
trip, failing `Unavailable` at the deadline and asking nothing once the budget is spent. A
route the budget ran out on dials nothing, wherever in the chain it was cut. A re-ask after a
takeover gets a fresh budget because its caller's first one died with the node that spent it.

The bound assumes the message leaves the receive buffer inside the budget. One that
waits there has no transaction and drew no 100, so its caller is still retransmitting
under Timer A and gives up by its own Timer B (§17.1.1.2).

`b2bua_new_call_turn_wait_seconds` reads the time from the receipt to the turn's start,
`b2bua_new_call_permit_hold_seconds` the time a turn holds its permits. Coverage:
`b2bua-harness/tests/it/setup_budget.rs`,
`failover-harness/tests/it/undecided_call_settled_on_departure.rs` (the re-ask's budget).

## Decision X2: one central "terminated-but-unanswered a-leg ⇒ 503" invariant

The funnel condition reads `before.a_leg.state ∈ {Trying, Early}`.

Guards (in order): a-leg unanswered entering the turn; no ≥200 response to the
a-leg among the turn's outbound effects (reject/relay/setup-timeout paths already
answer); no a-leg `Cancel` CDR (on CANCEL the txn layer autonomously answers
`487` — that *is* the final); non-empty a-leg INVITE snapshot. Idempotence
backstop: even if a guard is ever wrong, the a-leg server txn drops a second
final while `Completed` (`sip-txn::do_send_response`), so the worst case is a
harmless late raw datagram to a caller a *swept* (≥193 s old) txn already served.

X5 states why the `UnansweredCaller` split is a correctness requirement, not a
convenience.

## Decision X3: the canonical error-case status is 503, no Reason header

503 rather than 500: a 4xx states a caller fault and a 500 a malformed request the server
choked on, while 503 states a transient, retryable inability an upstream proxy or caller can
fail over on, and matches the TS reference and the Rust
overload and decision paths. The admission refusals are the overload and capacity gates, the
ingress brake and the deferred backlog. A `Reason` states the cause of a rejection someone
decided (RFC 3326), and a caller rerouting on a 5xx reads it as one. So whichever path minted
the 5xx (a spent budget, an unanswered consult, a refused route, the chain's depth, the X2
synthesis, a service's teardown), a stated `Reason` adds none. A decision's reject states its
own `Reason` and keeps it over the route's. The route's other stated headers ride as ever,
and a callee's relayed final takes them all.

This rule is the initial INVITE's alone: a re-INVITE or another request in a dialog is
answered by its own rules. Every final the stack mints for an initial INVITE is one of:
an internal error (this 5xx, no `Reason`); a status a decision engine or a peer stated;
the plain 500 refusing an engine's answer whose status is no final it may state; RFC
3261's own (400 §8.1.1.3, 482 §8.2.2.2, 483 §16.3, 487 §9.2); or the generic default
of a callee's silence (480 or 408: a no-answer deadline with no consult, a timeout let
stand, a relay treatment with no failure captured, the setup deadline). A deployment's
service may state otherwise. `b2bua-harness/tests/it/initial_invite_finals.rs` reads
them off the sources and pins each to its class, so a new one needs a row and a class.

## Decision X4: the LB proxy stays transaction-less; the caller's timer owns downstream silence

The front proxy is a **stateless UDP forwarder** (RFC 3261 §16.11). It **never
generates a 100** and it **relays the worker's 100** like every other response
(`core/response.rs`), so a caller routed through the LB hears the worker's 100
as it would on a direct path. A worker that never answers at all sends no 100:
the caller's own Timer A/B stays armed (§17.1.1), so **the caller** — not a
proxy timer — decides when a dead downstream is given up on (local 408 at 64·T1
≈ 32 s). The proxy therefore has
**no Timer C and no synthesized final** for a blackholed worker, *by design* —
adding one would re-introduce the per-INVITE proxy transaction state ADR-0009 /
ADR-0014 deliberately keep out of the LB (and duplicate a backstop the endpoints
already own). All proxy-*internal* errors do answer immediately with a final
(483/420/400/403, and a 503 with a Retry-After and no Reason for every shed path); the only
silent paths are pre-parse recv-queue overflow, unparseable datagrams, and the
downstream-blackhole case — all three bounded by the caller's transaction timer.

An INVITE response bound for a worker the proxy holds Dead fails over to the
backup its stickiness cookie names, read from the proxy's own Record-Route entry
or, on a response carrying none (the answer to a worker's in-dialog INVITE), from
the Via the proxy pushed on that INVITE (ADR-0009 X1). The survivor takes the
call over on it, and a round its replica holds completes there
(`failover-harness/tests/it/reinvite_answer_failover.rs`), or gives up at the
seeded transaction's INVITE bound when the callee never answers
(`reinvite_round_gives_up_after_takeover.rs`). A forged cookie routes nothing.
A non-INVITE response answers a client transaction only its sender holds
(§17.1.2) and follows the Via.

A caller that heard the worker's 100 sends no retransmission that could re-place
its call: its client transaction is `Proceeding`, which runs no Timer B. The
platform answers it without a message from it (ADR-0047): a new call is written
to its backup at its birth, one replication stream hop after the 100
(`failover-harness/tests/it/new_call_replication_window.rs`), and when its worker
leaves the membership set the backup takes over every call with an INVITE in
flight, answering one still waiting on its routing decision with a 503
(`undecided_call_settled_on_departure.rs`). A round whose relay has not
replicated yet is acknowledged when the callee's answer takes the call over, and
the call ended (`reinvite_round_not_yet_replicated.rs`); the caller's pending
re-INVITE of that round gets no final, only the BYE.

Coverage: `sip-proxy/tests/it/stateless_final_response_contract.rs` pins both halves
(no proxy 100 + the downstream 100, 18x and 200 relayed; blackhole → zero
proxy-originated upstream messages, retransmit re-forwarded to the same target),
and `sip-proxy/src/core/response/relay_tests.rs`
(`trying_100_is_relayed_upstream_by_its_via`) the relayed 100's Via pop.

## Decision X6: the per-call-cap shed: a new INVITE at capacity gets a 503, not a silent drop

The cap refusal counts as `cap_drop`. It happens before any call or transaction context is
born, so there is no live call for the deadline to bound and no `→ terminated` edge for the
synthesis to ride.

The refusal of a **new INVITE** there is the at-cap cell of its row in the
dispatch table (`dispatch/class.rs`): the offer hands the INVITE back owing the
new-call 503 (X3) — a To-tag, a jittered `Retry-After`, no `Reason`, no per-call
state born — which the router sends through the INVITE server txn that carried
the 100, counted as a cap-shed new call. The rest of the table answers the
other events refused there: a non-INVITE request has its server transaction
forgotten, so its retransmission is admitted again once the cap frees; an
in-dialog INVITE — silenced by its 100 like the initial one — is answered 500
with a `Retry-After`. The threshold check and the creation of a queue happen in
one offer under the dispatcher's map lock, so no other offer can take the last
place between them. This path is unreachable under sane tuning (the
ingress brake, the admission ladder's rungs, and call-cap all shed with proper 503s far
earlier); X6 exists so the guarantee is **total** — no INVITE that heard a 100
is ever left silent, at any scale. Coverage:
`decision_deadline.rs::initial_invite_at_the_per_call_cap_is_shed_503_not_dropped`
(cap = 1, one live call, next new INVITE → 503, no call born) and
`new_invite_at_queue_cap.rs` (the 503's shape and counting).

## Decision X5: HA interaction: reclaim-discharge stays OFF the SIP wire

This is the critical HA design point. The X2 synthesis must fire for calls the
**local node is actively serving** (its INVITE server txn is live and the caller
is waiting on *us*), and must **not** fire when a node discharges an
**already-terminal body it reclaimed or had folded to it** by HA replication:

- **Live-serving paths (`UnansweredCaller::Answer`).** initial-INVITE,
  the rule chain, limiter-refresh, and the reaper's `discharge_as_own` — here an
  unanswered a-leg means a caller is genuinely parked on our server txn. Answer.

- **HA discharge paths (`UnansweredCaller::Leave`).**
  `discharge_materialized_terminal` (reverse-flush reconcile, reboot
  bulk/on-demand reclaim of a `Terminated` body) and `discharge_folded_terminal`
  (a backup's deferred terminal folded into our live map). These bodies were
  already carried to terminal by **whichever node served them** — that node
  either already put the final on the wire, or its INVITE server transaction died
  with it (a crash ≥ `reboot_budget` ago). A takeover/reclaiming node has **no
  live server transaction for that a-leg**, and under ADR-0014 reactive-only
  takeover, reclaim-discharge is **causal, never time-based, and never touches the
  SIP wire** — it settles obligations (CDR, limiter) and propagates the delete,
  nothing more. Synthesizing a 503 here would put a spurious final onto a wire
  the reclaiming node does not own, toward a caller who is either long gone or
  already answered — re-introducing exactly the double-serve class ADR-0014
  structurally removed. So the off-wire endings pass `UnansweredCaller::Leave`.

The boundary is therefore the same one ADR-0020 X3 draws for the reaper (primary
live-served + rehydrated calls in scope; acting-backup takeover copies out of
scope) and ADR-0014 draws for reconciliation (`(p,b)`-causal, no clock): **the
node that holds the live transaction answers; the node that merely holds terminal
state settles silently.** The decision deadline (X1) further guarantees the
live-serving node *reaches* that terminal state within 5 s instead of parking a
limiter/txn slot until the 150 s `SetupTimeout` / crash. A node that dies mid-setup
leaves a live body, not a terminal one: whoever materialises it with no message —
the departure sweep or the restarted node's reclaim — rebuilds the caller's INVITE
server transaction and answers it 503, so it holds the live transaction and answers
(ADR-0047 D4).

## Consequences

- `b2bua-harness/tests/decision_deadline.rs` pins (a) a hung engine → 503 at the default 5 s
  deadline, bracketed to prove the bound, and (b) a panicking engine → 503 immediately via
  the reaper strike-1 + X2 synthesis. `reaper.rs::wedged_setup_is_aborted_and_reaped` runs
  with the deadline disabled, to exercise the abort ladder, and the reap answers the caller
  a late 503.
- `invariants::enforce` takes `now_ms` and `UnansweredCaller` at its seven call
  sites. The verbatim limiter/CDR-extraction property test pins the additive nature by
  passing `UnansweredCaller::Leave`.
- A call-control adapter may set a tighter internal timeout for its own resource hygiene.
- X4 is a codified invariant of the LB proxy and its regression test, with no change to the
  proxy's behaviour.

## Alternatives considered

- **Timeout inside a ported HTTP adapter (TS-faithful).** Rejected as the *sole*
  mechanism: it does not bind third-party adapters and leaves the panic/reaper
  silence gaps open. The core deadline (X1) + central synthesis (X2) cover all
  engines and all internal-failure paths; a per-adapter timeout becomes an
  optional optimisation, not the guarantee.
- **Answer 503 from each backstop (reaper rule, panic hook, txn sweep)
  individually.** Rejected: three-plus code paths to keep in sync, and the txn
  sweep has no call context. One invariant at the shared funnel is the ADR-0020
  "one funnel" discipline applied to the caller-facing final.
- **A Timer C in the LB proxy.** Rejected (X4): duplicates the endpoint's own
  transaction timer and re-introduces per-INVITE proxy state.
- **500 instead of 503.** Rejected (X3): 503 is transient/retryable and matches
  existing behaviour and the TS reference.
