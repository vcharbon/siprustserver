# A worker's two readiness axes: serving dialogs, accepting new calls

**Status:** accepted.

**Owner:** the worker's state in `crates/b2bua/src/repl/readiness.rs`, the proxy's view in
`crates/sip-proxy/src/registry/` and `health/probe.rs`; the full list is in the
[appendix](appendix/0050-a-workers-two-readiness-axes.md), with each section in full.

Amends ADR-0043 §11–12, ADR-0031 D1 and D4, ADR-0046 §2 and D4, ADR-0012 D4, ADR-0014 §5 and
ADR-0009 (the fresh-pod guard and the drain grace).
Detail: [appendix, amended](appendix/0050-a-workers-two-readiness-axes.md#amended).

## Context

One "ready or not" bit on a worker's OPTIONS answer answered two questions: may it take a new
call, and may an in-dialog request reach it. A gating backend's outage then failed every
in-dialog request of the cluster with `503`, or forced takeovers of calls a worker still held.
The fresh-pod guard diverted dialogs for 20 s after any join or proxy restart. A starting proxy
answered `503` to everything. No state refused new calls while keeping established ones.
Detail: [appendix, context](appendix/0050-a-workers-two-readiness-axes.md#context).

## Decision

1. **Two axes.** A worker is **serving dialogs** while its process is up and holds its calls,
   and **accepting new calls** while it takes an initial INVITE. The proxy reads the first as
   liveness and the second as acceptance, and never derives one from the other.
2. **The OPTIONS answer.** `200` whenever the worker serves dialogs, with `X-Overload`,
   `X-Readiness: new=accept|refuse; cause=…` and `X-Incarnation`; `503` only while NotReady.
   The proxy acts on `new` alone. No transaction is held for an out-of-dialog OPTIONS.
   [Detail](appendix/0050-a-workers-two-readiness-axes.md#2-the-options-answer).
3. **The proxy's view of a worker** is health `Unknown | Alive | Dead` plus acceptance. A turn
   to `Dead` from `Alive` (or a flip of an `Alive` worker to not ready, not terminating, with
   another member ready) is a **dead verdict**, told to every other `Alive` worker on a signed
   `X-Worker-Dead` notice. A worker takes a verified notice fresh within
   `B2BUA_DEAD_NOTICE_FRESH_MS` (10 s), holds the verdict `B2BUA_DEAD_VERDICT_HOLD_MS` (42 s), and
   takes over the dead worker's calls once its own stream from it is down (ADR-0047 D1, D2).
   [Detail](appendix/0050-a-workers-two-readiness-axes.md#3-the-proxys-view-of-a-worker).
4. **In-dialog routing reads liveness only.** A cookied request goes to `w_pri` while `Alive`,
   else to `w_bak` while `Alive`, never to a fresh selection. An `Unknown` worker is not routed.
   A worker that refuses new calls keeps every dialog it owns.
   [Detail](appendix/0050-a-workers-two-readiness-axes.md#4-in-dialog-routing-reads-liveness-only).
5. **No answer without a target.** An in-dialog request with no `Alive` owner, or before the
   first worker view, is dropped and counted, never answered `503`. A worker receiving a copy of
   a replicated call's initial INVITE takes the call over; a foreign INVITE with no record gets
   no final. [Detail](appendix/0050-a-workers-two-readiness-axes.md#5-no-answer-without-a-target).
6. **New calls.** A new dialog goes to an `Alive` accepting worker, else to an `Alive` refusing
   one, which answers its own refusal and writes its call record. The proxy refuses a new call
   only from its own overload gates, never from a worker's state. [Detail](appendix/0050-a-workers-two-readiness-axes.md#6-new-calls).
7. **The cookie's `w_bak`** is the rendezvous winner among `Alive` accepting workers other than
   the primary, among all `Alive` ones when none accepts, else among the known ordinals.
   [Detail](appendix/0050-a-workers-two-readiness-axes.md#7-the-cookies-w_bak).
8. **No fresh-pod guard.** A worker joins `Unknown` and is routed once a probe answers `200`. A
   changed `X-Incarnation` resets it to `Unknown`.
   [Detail](appendix/0050-a-workers-two-readiness-axes.md#8-no-fresh-pod-guard).
9. **The worker's states.** NotReady (`503`); Ready (accepting, unless a gating backend's
   breaker or the event-queue gate refuses); Cordoned (refuses new calls, serves the rest,
   reversible); Draining (SIGTERM or withdrawal, terminal, ADR-0031 D2).
   [Detail](appendix/0050-a-workers-two-readiness-axes.md#9-the-workers-states).
10. **Operator routes** on a loopback listener (`B2BUA_ADMIN`): `/admin/cordon` (POST, DELETE,
    GET) and `POST /admin/release-all`, which closes every call's record without signalling.
    [Detail](appendix/0050-a-workers-two-readiness-axes.md#10-operator-routes).
11. **The proxy's readiness.** `/readyz` is Ready when SIGTERM has not latched, no recv shard is
    stalled and one worker is `Alive`; acceptance plays no part. The proxy probes every member
    present and not `terminating`. While none reads ready, it routes on the probe's answers.
    [Detail](appendix/0050-a-workers-two-readiness-axes.md#11-the-proxys-readiness).
12. **The keepalive lane.** The transaction layer passes each out-of-dialog OPTIONS up on a
    small bounded lane apart from the event queue, taken first by the router, so a probe never
    waits behind the queued events.
    [Detail](appendix/0050-a-workers-two-readiness-axes.md#12-the-keepalive-lane).

## Consequences

- A backend outage refuses new calls at the workers, each with its call record, and touches no
  established call.
- A restarted worker's new calls stay on it; a restarted proxy diverts nothing once its first
  probe round answered.
- A site leaves service by cordon, then SIGTERM or release-all.
- An in-dialog request with no live worker gets no answer; the sender's timers end it.
- A starved worker reads refusing, keeps its dialogs, and is never taken over for it.
- Open: "none ready" is read literally, so a worker the slice reads ready but the probe reads
  `Dead`, beside a not-ready worker that answers, leaves no target for about 50 s.

Tests and the long form: [appendix, consequences](appendix/0050-a-workers-two-readiness-axes.md#consequences).
