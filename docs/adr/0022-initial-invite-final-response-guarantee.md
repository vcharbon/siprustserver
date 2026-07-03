# 0022 — The initial-INVITE final-response guarantee (accept-and-forward, or 503 within the decision deadline)

**Status:** accepted (2026-07-03)

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
`b2bua::decision::DeadlineDecisionEngine::wrap(engine, call_control_timeout_ms)`
wraps whatever engine the host injects, at `B2buaCore::spawn_with_overload`,
*after* the harness `tune` seam has finalized the config — so no injection path
(tests included) can bypass it. Each wrapped call rides `tokio::time::timeout`
(deterministic under `start_paused`); on expiry it returns
`CallDecisionError::Unavailable`, which every call-site already maps to a final
response. The expired inner future is dropped — for an HTTP adapter that cancels
the in-flight request, exactly like the TS `Effect.timeoutOrElse`.

Config: `call_control_timeout_ms`, default **5000** (TS `CALL_CONTROL_*_TIMEOUT_MS`
parity), env `B2BUA_CALL_CONTROL_TIMEOUT_MS`; `<= 0` disables (the escape hatch
the reaper wedge test uses to exercise the abort-escalation ladder against a
*genuinely* wedged await).

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
complete call setup: decision-backend error/timeout, target-admission reject,
bogus-route, and the X2 unanswered-a-leg synthesis. Rationale: it is transient
and retryable (unlike a 4xx caller-fault or a 500 "malformed request the server
choked on"), it matches the TS reference and the existing Rust overload/decision
paths, and it lets an upstream proxy or caller fail over. The X2 synthesis and
the decision-error reject carry **no Reason header** (the bare canonical reject);
the *overload* 503 deliberately still carries `Reason: SIP;cause=503` +
`Retry-After` because those inform the LB's AIMD control loop.

## Decision X4 — the LB proxy stays transaction-less; the caller's timer owns downstream silence

The front proxy is a **stateless UDP forwarder**. It **never generates a 100** and
it **absorbs the worker's hop-by-hop 100** (`core/response.rs`), so a caller
routed through the LB receives *no* provisional until a real 18x. That absence is
load-bearing: with no provisional, the caller's own Timer A/B stays armed (RFC
3261 §17.1.1), so **the caller** — not a proxy timer — decides when a dead
downstream is given up on (local 408 at 64·T1 ≈ 32 s). The proxy therefore has
**no Timer C and no synthesized final** for a blackholed worker, *by design* —
adding one would re-introduce the per-INVITE proxy transaction state ADR-0009 /
ADR-0014 deliberately keep out of the LB (and duplicate a backstop the endpoints
already own). All proxy-*internal* errors do answer immediately with a final
(483/420/400/403, and 503+Retry-After+Reason for every shed path); the only
silent paths are pre-parse recv-queue overflow, unparseable datagrams, and the
downstream-blackhole case — all three bounded by the caller's transaction timer.

Coverage: `sip-proxy/tests/stateless_final_response_contract.rs` pins both halves
(no proxy 100 + worker-100 absorbed + 18x/200 relay; blackhole → zero
proxy-originated upstream messages, retransmit re-forwarded to the same target).

## Decision X6 — the per-call-cap shed: a new INVITE at capacity gets a 503, not a silent drop

`PerCallDispatcher::dispatch` silently drops (and counts, `bump_cap_drop`) a
brand-new call_ref's body when the live-queue map is at `per_call_queue_cap`
(default 200 000). This is the **one** full-queue path X1 and X2 cannot reach:
the drop happens in the dispatcher *before* any call/txn context is born, so
there is no live call for the deadline to bound and no `→ terminated` edge for the
synthesis to ride — the caller heard the auto-100 and then nothing.

`router::on_event` closes it: for an **initial INVITE** (`res.initial_invite`)
whose new call_ref `would_drop_new_at_cap`, it sends a **stateless 503** through
the INVITE server txn (the same shape as the Tier-3 admission gate —
`build_stateless_overload_503`, no per-call state born) and returns, before
`dispatch`. In-dialog events for an at-cap new call_ref keep the silent
`dispatch` cap-drop: an in-dialog request with no live call is an orphan the peer
481s / the protocol resends; only the initial INVITE owes a final. The check is
race-free from the single-task router — only the router inserts queues, so between
`would_drop_new_at_cap` and the following `dispatch` the count can only fall (a
worker finishing), never rise. This path is unreachable under sane tuning (the
Tier-1 ingress brake, Tier-3 CPS gate, and call-cap all shed with proper 503s far
earlier); X6 exists so the guarantee is **total** — no INVITE that heard a 100 is
ever left silent, at any scale. Coverage:
`decision_deadline.rs::initial_invite_at_the_per_call_cap_is_shed_503_not_dropped`
(cap = 1, one live call, next new INVITE → 503, no call born).

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
limiter/txn slot until the 150 s `SetupTimeout` / crash — the ledger-replicated
`SetupTimeout` remains the cross-crash backstop for a node that dies mid-setup
(its caller's txn dies with it, so no wire answer is owed).

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
