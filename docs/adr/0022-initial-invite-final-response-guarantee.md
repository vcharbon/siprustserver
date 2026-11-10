# 0022. The initial-INVITE final-response guarantee (accept-and-forward, or 503 within the decision deadline)

**Status:** accepted (2026-07-03). Amended with ADR-0047 (X4, X5) and ADR-0055 §3 (X1).

The gaps the decision closes, the long form of each section, its coverage and the rejected
alternatives: [appendix](appendix/0022-initial-invite-final-response-guarantee.md).

## Context

The operational contract:

> Once the platform has sent a **100 Trying** back to a caller for an initial INVITE, that
> call is either **accepted and forwarded**, or **rejected with a final `503` (no dialog)
> within the decision deadline (default 5 s)**, for *any* internal failure: a
> decision-backend timeout, an HTTP call-control endpoint that hangs, an overload reject, or
> an internal panic. The caller is never left on "100-then-silence".

sip-txn sends the 100 before the router and the decision engine run. Behind it, a consult
with no deadline or a silent backstop (reaper, panic, transaction sweep) breaks the contract.
The transaction-less LB proxy cannot (X4). Detail: [appendix, context](appendix/0022-initial-invite-final-response-guarantee.md#context-the-three-gaps).

## Decision X1: a core-level decision deadline (`DeadlineDecisionEngine`)

`b2bua::decision::DeadlineDecisionEngine::wrap(engine, config.call_control_timeout())` wraps
the injected engine at `B2buaCore::spawn_with_overload`, after the harness `tune` seam, so no
injection path bypasses it. On expiry a call returns
`CallDecisionError::Unavailable`.

Config `call_control_timeout_ms`, default 5000, env `B2BUA_CALL_CONTROL_TIMEOUT_MS`. It is a
deadline, never a switch. `B2buaConfig::validate` refuses `<= 0`, and the one reader,
`B2buaConfig::call_control_timeout`, gives a harness config that writes it anyway the default.

It bounds `new_call` and `call_failure`. `call_refer` passes through, bounded by the REFER
subscription timers.

One setup budget runs from the INVITE's receipt (`TransactionEvent::Message::opened_at`) to
its final (`b2bua::setup_budget`). It covers the wait for the handler permits, the decision
round trips with their retries, the limiter's admits and the failover chain they start.

- **At the permit.** A turn stops waiting at the deadline and runs holding no permit. A turn
  whose budget is spent as it starts asks nothing: its caller gets the X3 503, counted
  `setup_budget` on `b2bua_new_calls_total`. A setup its caller CANCELed first ends as
  cancelled. No ADR-0037 rung reads the budget, and an emergency INVITE is held to it too.
- **On the turn.** A consult is bounded by what is left of the budget, and each route's admit
  is cut there as an unavailable limiter. A route the budget ran out on ends `Refused`, its
  caller gets the bare 503 (ADR-0043 §10), and the record owes the release of any admit
  that left (ADR-0040).

Under the `applied` sync level the final may leave up to the sync cap after the deadline
(ADR-0055 §3). A re-ask after a takeover (ADR-0047 D4) gets a fresh budget from its notice.
The off-turn `/call/failure` consult keeps one deadline per round trip under its answer
deadline (ADR-0039). Detail: [appendix X1](appendix/0022-initial-invite-final-response-guarantee.md#decision-x1-a-core-level-decision-deadline-deadlinedecisionengine).

## Decision X2: one central "terminated-but-unanswered a-leg ⇒ 503" invariant

`invariants::enforce`, the funnel every termination rides, closes the guarantee on the
`→ terminated` edge. If the a-leg entered the turn unanswered (`Trying` or `Early`) and
nothing on the turn answered it, the funnel appends `503 Service Unavailable` with no Reason
header, and a `Reject`/`503` CDR event (`reason = "unanswered_at_termination"`). `enforce` takes
`UnansweredCaller`, `Answer` on every live-serving funnel and `Leave` on the two HA
discharge helpers (X5). Detail: [appendix X2](appendix/0022-initial-invite-final-response-guarantee.md#decision-x2-one-central-terminated-but-unanswered-a-leg--503-invariant).

## Decision X3: the canonical error-case status is 503, no Reason header

`503 Service Unavailable` with no Reason is the one code for every server-side inability to
complete call setup: decision error or timeout, destination allow-list reject, bogus route,
and the X2 synthesis. A new INVITE refused at admission gets the 503 with a jittered `Retry-After`
(ADR-0037 item 3). A route the limiter refused with no way to fail over (past
`MAX_LIMITER_FAILOVER` or with no callback context, on the turn or off it) gets the same bare
503, and its record still owes the release of the admits that left.

A route's stated `Reason` never rides a 5xx this stack authors for the initial INVITE: the
end-of-turn stamp (`rules::stated_headers`) leaves `Reason` as it finds it. A final that
states its own keeps it. The rule is the initial INVITE's alone. Every final the stack
mints for an initial INVITE has a class, pinned by `initial_invite_finals.rs`. Detail:
[appendix X3](appendix/0022-initial-invite-final-response-guarantee.md#decision-x3-the-canonical-error-case-status-is-503-no-reason-header).

## Decision X4: the LB proxy stays transaction-less; the caller's timer owns downstream silence

The front proxy is a stateless UDP forwarder (RFC 3261 §16.11). It never generates a 100 and
relays the worker's 100 like any response. It has no Timer C and no synthesized final for a
blackholed worker: the caller's Timer A/B decides (ADR-0009, ADR-0014). Every proxy-internal
error answers at once with a final. An INVITE response bound for a Dead worker fails over on
the cookie of the proxy's Record-Route or of its Via. A caller that heard the 100 is answered
by the backup's takeover (ADR-0047). Detail: [appendix X4](appendix/0022-initial-invite-final-response-guarantee.md#decision-x4-the-lb-proxy-stays-transaction-less-the-callers-timer-owns-downstream-silence).

## Decision X6: the per-call-cap shed: a new INVITE at capacity gets a 503, not a silent drop

The per-call dispatcher refuses an event for a call with no queue once the live-queue map is
at `per_call_queue_cap` (default 200 000), a normal new INVITE at the cap less the new-call
headroom (ADR-0037 item 10). No call exists there for X1 or X2 to reach. A refused new INVITE
gets the X3 503 with a To-tag and a jittered `Retry-After`, through the server transaction
that carried the 100, counted as a cap-shed new call. Detail: [appendix X6](appendix/0022-initial-invite-final-response-guarantee.md#decision-x6-the-per-call-cap-shed-a-new-invite-at-capacity-gets-a-503-not-a-silent-drop).

## Decision X5: HA interaction: reclaim-discharge stays OFF the SIP wire

The node that holds the live transaction answers. The node that merely holds terminal state
settles silently. The live-serving funnels (initial-INVITE, the rule chain, limiter-refresh,
the reaper's `discharge_as_own`) pass `UnansweredCaller::Answer`.
`discharge_materialized_terminal` and `discharge_folded_terminal` pass `false`: they settle
CDR and limiter obligations and propagate the delete, never touching the SIP wire. A node
that dies mid-setup leaves a live body, and whoever materialises it rebuilds the caller's
server transaction and answers it 503 (ADR-0047 D4). Detail: [appendix X5](appendix/0022-initial-invite-final-response-guarantee.md#decision-x5-ha-interaction-reclaim-discharge-stays-off-the-sip-wire).

## Consequences

- The guarantee is structural and pinned by `b2bua-harness/tests/decision_deadline.rs`.
- A call-control adapter needs no timeout of its own to be safe.

## Alternatives considered

- A timeout inside the HTTP adapter alone. Rejected: it binds no third-party adapter.
- A 503 from each backstop. Rejected: several paths to keep in sync, one with no call context.
- A Timer C in the LB proxy. Rejected (X4).
- 500 instead of 503. Rejected (X3).

Long form: [appendix](appendix/0022-initial-invite-final-response-guarantee.md#alternatives-considered).
