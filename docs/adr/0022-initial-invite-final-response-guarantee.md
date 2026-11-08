# 0022 — The initial-INVITE final-response guarantee (accept-and-forward, or 503 within the decision deadline)

**Status:** accepted (2026-07-03). Amended (2026-10-12): X4, the front proxy relays the worker's 100, and an
INVITE response bound for a dead worker fails over on the cookie of its Record-Route or of the proxy's Via.
Amended (2026-10-12): X4 and X5, a caller that heard the 100 is answered when its worker departs (ADR-0047).
Amended (2026-10-23): the first provisional of an INVITE leaves after the call's write,
up to a cap (ADR-0055 §3).
Amended (2026-11-04): X1, one setup budget from the INVITE's receipt to its final; X3, a limiter
chain that ends with no route is an internal error, and every final the stack mints for an initial
INVITE has a class.

## Context

The operational contract we want to be able to state without hedging:

> Once the platform has sent a **100 Trying** back to a caller for an initial
> INVITE, that call is either **accepted and forwarded**, or **rejected with a
> final `503` (no dialog) within the decision deadline (default 5 s)** — for
> *any* internal failure: a decision-backend timeout, an HTTP call-control
> endpoint that hangs, an overload reject, or an internal panic. The caller is
> never left on "100-then-silence".

A 2026-07-03 review found the guarantee did **not** hold. Three independent gaps:

1. **No deadline at the decision seam.** sip-txn auto-sends `100 Trying` the
   instant an INVITE server transaction is born (`layer.rs`,
   `handle_inbound_request`), *before* the router or the decision engine run. The
   initial-INVITE handler then `await`s `decision.new_call()` **bare** — no
   timeout (`initial_invite.rs`). The 5 s bound existed only in the TS
   `HttpReferenceAdapter` (`callControlNewCallTimeoutMs = 5000`), which was never
   ported. A hung or slow HTTP call-control endpoint — including any **third-party
   `CallDecisionEngine`** that forgot its own timeout — parked the per-call worker
   indefinitely behind the caller's 100.

2. **Every backstop behind the seam was silent toward the caller.** When a call
   reached `→ terminated` with its a-leg INVITE still unanswered — the reaper's
   `reaper-stale` / `reaper-fatal-error` / strike-2 discharge paths
   (`defaults.rs::reap_force_terminal` deliberately emits **no** wire messages),
   a handler **panic** during INVITE handling, or the sip-txn safety sweep
   silently `delete_txn`-ing an unanswered server txn at ~193 s
   (`layer.rs::sweep`) — the caller heard 100 and then nothing. This was even
   *pinned as expected* by `reaper.rs::wedged_setup_is_aborted_and_reaped`
   ("the txn layer's 100 Trying is all alice ever hears").

3. **Status-code drift.** Decision-error and overload rejects use `503`; the
   requirement was informally stated as "500". Resolved in favour of **503
   Service Unavailable** (see Decision X3) — it is what both the Rust port and
   the TS reference already emit, and it is the semantically correct code for a
   transient server-side inability to handle the call.

The front **LB proxy** is a separate, already-correct story (Decision X4): it is
deliberately transaction-less and cannot produce "100-then-silence".

## Decision X1 — a core-level decision deadline (`DeadlineDecisionEngine`)

The bound is enforced in the **core**, not in the adapter, so it holds
*structurally* no matter how the injected `CallDecisionEngine` is written.
`b2bua::decision::DeadlineDecisionEngine::wrap(engine, config.call_control_timeout())`
wraps whatever engine the host injects, at `B2buaCore::spawn_with_overload`,
*after* the harness `tune` seam has finalized the config — so no injection path
(tests included) can bypass it. Each wrapped call rides `tokio::time::timeout`
(deterministic under `start_paused`); on expiry it returns
`CallDecisionError::Unavailable`, which every call-site already maps to a final
response. The expired inner future is dropped — for an HTTP adapter that cancels
the in-flight request, exactly like the TS `Effect.timeoutOrElse`.

Config: `call_control_timeout_ms`, default **5000** (TS `CALL_CONTROL_*_TIMEOUT_MS`
parity), env `B2BUA_CALL_CONTROL_TIMEOUT_MS`. It is a deadline, never a switch:
`B2buaConfig::validate` refuses `<= 0` at startup, and the one reader,
`B2buaConfig::call_control_timeout`, gives a harness config that writes it anyway
the default. No consult runs unbounded while it holds its call's lock.

**Scope — `new_call` + `call_failure`, deliberately NOT `call_refer`.** These two
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

**One setup budget, from the INVITE's receipt to its final** (`b2bua::setup_budget`).
On an initial INVITE's turn `call_control_timeout_ms` bounds the whole setup, not each
round trip: a caller behind the 100 waits for all of it together. The budget runs from
the instant the transaction layer opened the INVITE's server transaction
(`TransactionEvent::Message::opened_at`), through the event queue and the router, and a
retransmission moves nothing: the transaction absorbs it (RFC 3261 §17.2.3). A copy on a
new branch (§8.2.2.2) opened a transaction of its own and heard its own 100: it is no new
call, but its turn stops waiting at its own budget and answers its 482. It covers the wait for the turn's
handler permits, the decision round trips with their retries, the limiter's admits and
the failover chain they start. It has one check and one deadline:

- **At the permit.** A turn stops waiting at the deadline and runs holding no permit
  (`Runnable::due_by`). One whose budget is spent as it starts asks nothing: its caller
  gets the 503 of an unavailable decision (X3), its call born only to write its record,
  and the INVITE is counted once as `setup_budget` on `b2bua_new_calls_total`. A setup
  its caller CANCELed first ends as cancelled, with no second final. This is no admission
  rung: no rung of ADR-0037 reads it, and an emergency INVITE is held to it too, since a
  caller left waiting serves nobody.
- **On the turn.** `DeadlineDecisionEngine` is the one bound on a round trip: a consult
  on a setup's turn is bounded by what is left of the budget and fails `Unavailable` at
  the deadline, asking nothing once it is spent, and the admit of each route is cut there
  as an unavailable limiter. A route the budget ran out on dials nothing: it ends
  `Refused`, the caller gets the bare 503 wherever in the chain it was cut (ADR-0043
  §10), and the call's record owes the release of any admit that left (ADR-0040).

The final leaves through the call's outbox, so under the `applied` sync level it may
leave up to the sync cap after the deadline (ADR-0055 §3). A re-ask after a takeover
(ADR-0047 D4) gets a fresh budget from its notice: its caller's first one died with the
node that spent it. The `/call/failure` consult after a b-leg's failure runs off the turn
and keeps one deadline per round trip under its answer deadline (ADR-0039).

The bound assumes the message leaves the receive buffer inside the budget. One that
waits there has no transaction and drew no 100, so its caller is still retransmitting
under Timer A and gives up by its own Timer B (§17.1.1.2).

`b2bua_new_call_turn_wait_seconds` reads the time from the receipt to the turn's start,
`b2bua_new_call_permit_hold_seconds` the time a turn holds its permits. Coverage:
`b2bua-harness/tests/it/setup_budget.rs`,
`failover-harness/tests/it/undecided_call_settled_on_departure.rs` (the re-ask's budget).

## Decision X2 — one central "terminated-but-unanswered a-leg ⇒ 503" invariant

Rather than patch each silent backstop, the guarantee is closed **once** at the
single funnel every termination rides — `invariants::enforce`, on the
`→ terminated` edge. If the a-leg entered the turn unanswered
(`before.a_leg.state ∈ {Trying, Early}`) and nothing on the turn answered it, the
funnel appends the final the path forgot: **`503 Service Unavailable`**, no
Reason header, plus a `Reject`/`503` CDR event
(`reason = "unanswered_at_termination"`). This subsumes the earlier point-local
message-cap fix and covers the reaper, panic, discharge, and decision-error
paths uniformly.

Guards (in order): a-leg unanswered entering the turn; no ≥200 response to the
a-leg among the turn's outbound effects (reject/relay/setup-timeout paths already
answer); no a-leg `Cancel` CDR (on CANCEL the txn layer autonomously answers
`487` — that *is* the final); non-empty a-leg INVITE snapshot. Idempotence
backstop: even if a guard is ever wrong, the a-leg server txn drops a second
final while `Completed` (`sip-txn::do_send_response`), so the worst case is a
harmless late raw datagram to a caller a *swept* (≥193 s old) txn already served.

The `enforce` signature gains `now_ms` and an `answer_unanswered_a_leg: bool`.
The flag is **`true` on every live-serving funnel** (rules, initial-INVITE,
limiter-refresh, reaper discharge-as-own) and **`false` on the two HA discharge
helpers for already-terminal reclaimed/folded bodies**
(`discharge_materialized_terminal`, `discharge_folded_terminal`). See X5 for why
that split is a correctness requirement, not a convenience.

## Decision X3 — the canonical error-case status is 503, no Reason header

`503 Service Unavailable` is the single code for every server-side inability to
complete call setup: decision-backend error/timeout, destination allow-list reject,
bogus-route, and the X2 unanswered-a-leg synthesis. Rationale: it is transient
and retryable (unlike a 4xx caller-fault or a 500 "malformed request the server
choked on"), it matches the TS reference and the existing Rust overload/decision
paths, and it lets an upstream proxy or caller fail over. The X2 synthesis and
the decision-error reject carry **no Reason header** (the bare canonical reject).
A new INVITE refused at admission — the overload and capacity gates, the
ingress brake, the deferred backlog — gets the 503 with a jittered
`Retry-After` and no `Reason` either (ADR-0037 item 3). A route the limiter refused with
no way to fail over, past `MAX_LIMITER_FAILOVER` or with no callback context, on the
initial INVITE's turn or off it (ADR-0039), is no capacity statement to its caller: it
gets the same bare 503, and its record still owes the release of the admits that left.

A route's stated `Reason` never rides this 503. A `Reason` states the cause of a
rejection someone decided (RFC 3326), and a caller rerouting on a 5xx reads it as one. So
the end-of-turn stamp (`rules::stated_headers`) leaves `Reason` as it finds it on any 5xx
this stack authors for the initial INVITE: whichever path minted it (a spent budget, an
unanswered consult, a refused route, the chain's depth, the X2 synthesis, a service's
teardown), a stated `Reason` adds none. A final that states its own, as a decision's
reject does, keeps it over the route's. The route's other stated headers ride as ever,
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

## Decision X4 — the LB proxy stays transaction-less; the caller's timer owns downstream silence

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

## Decision X6 — the per-call-cap shed: a new INVITE at capacity gets a 503, not a silent drop

The per-call dispatcher refuses an event for a call with no queue once the
live-queue map is at `per_call_queue_cap` (default 200 000), a normal new
INVITE already at the cap less the new-call headroom (ADR-0037 item 10),
counting it (`cap_drop`). This is the **one** full-queue path X1 and X2 cannot reach: the
refusal happens *before* any call/txn context is born, so there is no live call
for the deadline to bound and no `→ terminated` edge for the synthesis to ride.

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

## Decision X5 — HA interaction: reclaim-discharge stays OFF the SIP wire

This is the critical HA design point. The X2 synthesis must fire for calls the
**local node is actively serving** (its INVITE server txn is live and the caller
is waiting on *us*), and must **not** fire when a node discharges an
**already-terminal body it reclaimed or had folded to it** by HA replication:

- **Live-serving paths (`answer_unanswered_a_leg = true`).** initial-INVITE,
  the rule chain, limiter-refresh, and the reaper's `discharge_as_own` — here an
  unanswered a-leg means a caller is genuinely parked on our server txn. Answer.

- **HA discharge paths (`answer_unanswered_a_leg = false`).**
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
  structurally removed. So the HA discharge helpers pass `false`.

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

- The guarantee is now structural and testable:
  `b2bua-harness/tests/decision_deadline.rs` pins (a) a hung engine → 503 at the
  default 5 s deadline, bracketed to prove the bound, and (b) a panicking engine
  → 503 immediately via the reaper strike-1 + X2 synthesis.
  `reaper.rs::wedged_setup_is_aborted_and_reaped` is updated: with the deadline
  *disabled* (to exercise the abort ladder) the reap now answers the caller a
  late 503 instead of staying silent.
- `invariants::enforce` grows two parameters; all seven call-sites updated. The
  verbatim limiter/CDR-extraction property test pins the additive nature by
  passing `answer_unanswered_a_leg = false`.
- Third-party call-control adapters no longer need to implement their own
  timeout to be safe — the core enforces the caller-facing bound regardless. An
  adapter *may* still set a tighter internal timeout for its own resource
  hygiene.
- No change to the LB proxy behaviour; X4 is a codified invariant + regression
  test, not a code change.

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
