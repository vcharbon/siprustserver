# A worker's two readiness axes: serving dialogs, accepting new calls

**Status:** accepted (2026-10-16). Amended (2026-10-28): §3, the proxy tells
each dead verdict to the workers; §5, the undecided call's residual.

**Owner:** `crates/sip-message` (the `X-Readiness` and `X-Incarnation` headers),
`crates/b2bua/src/repl/readiness.rs` and `router/responses.rs` (the worker's
state and its OPTIONS answer), `crates/b2bua-runner-kit/src/probe.rs` (the
admin routes), `crates/sip-proxy/src/registry/` and `health/probe.rs` (the
proxy's view of a worker), `strategies/load_balancer.rs` (target selection,
the cookie's `w_bak`), `load_observer/observer.rs` (the stale-payload sweep),
`crates/sip-proxy/src/health/dead_notice.rs`, `crates/sip-message`'s
`dead_notice.rs` and `crates/b2bua/src/router/dead_verdict.rs` (the dead
notice),
`crates/sip-proxy-runner/src/readiness.rs` and `main.rs` (`/readyz`, the boot
probe), the proxy chart's VRRP check.

Amends ADR-0043 §11–12, ADR-0031 D4, ADR-0046 §2 and D4, ADR-0012 D4,
ADR-0014 §5 and ADR-0009 (the fresh-pod guard and the drain grace).

## Context

A worker told the proxy one thing about itself, "ready or not", on its OPTIONS
answer, and the proxy used it for two different questions: may this worker
take a new call, and may an in-dialog request of a call it owns reach it.

- A gating backend's open breaker made a worker answer OPTIONS `503`
  not-ready (ADR-0043 §11). The proxy treated a not-ready primary like a dead
  one for in-dialog requests, sent them to `w_bak` only when the backup was
  `Alive`, and otherwise ran a fresh selection that never falls back for an
  in-dialog request. With the backend down for the whole cluster, every
  in-dialog request of every established call, the BYE included, got `503`
  `no_target_available`, and the calls stayed up on the workers. With one
  worker gated, its in-dialog requests went to its backup and forced a
  takeover of calls the worker still held.
- The `503` carried no load payload, so the proxy's stale-payload sweep
  floored the fallback worker's admission cap to 1 cps and the proxy refused
  new calls itself: no worker saw them, and no record of them was written.
- The fresh-pod guard sent every in-dialog request but ACK and CANCEL to
  `w_bak` for 20 s after the proxy first saw a worker. The cookie names no
  incarnation, so the guard also diverted the dialogs a worker created after
  it joined, and, after a proxy restart, every dialog of every worker. The
  case it was built for (a worker routed before its first OPTIONS answer) is
  covered by `Unknown` health and by the worker's own Ready gate, which
  latches only once its partition is bootstrapped and current.
- A freshly started proxy answered `503` to everything, in-dialog requests
  included, until its first probe round, a full probe interval after boot.
- Taking a worker or a whole site out of service cleanly needs a state that
  refuses new calls and keeps serving established ones for as long as the
  operator wants. The only "no new calls" state was the terminal `Draining`
  of a SIGTERM, which hands every call over within seconds.

## Decision

1. **Two axes.** A worker is **serving dialogs** while its process is up and
   holds its calls, and **accepting new calls** while it takes an initial
   INVITE. The proxy reads the first as liveness and the second as
   acceptance, and never derives one from the other.
2. **The OPTIONS answer.** A worker answers its peer proxy's OPTIONS `200`
   whenever it serves dialogs (Ready, Cordoned, Draining), always with its
   load payload (`X-Overload`), its `X-Readiness`, and its `X-Incarnation`
   (the process's boot generation, ADR-0011 X9). It answers `503` only while
   NotReady (not yet bootstrapped), with its `X-Incarnation` and no payload.

   ```
   X-Readiness: new=accept
   X-Readiness: new=refuse; cause=backend
   X-Readiness: new=refuse; cause=cordoned
   X-Readiness: new=refuse; cause=draining
   ```

   `cause` is free text for metrics and logs; the proxy acts on `new` alone.

   The worker holds no transaction for an out-of-dialog OPTIONS: every copy
   is answered afresh under a To-tag derived from the request (RFC 3261
   §8.2.7), so a retransmitted probe can draw a newer payload or readiness
   than the copy before it.
3. **The proxy's view of a worker** is health `Unknown | Alive | Dead` plus
   acceptance. A `200` makes it `Alive` and sets acceptance from `new` (an
   absent header reads `accept`); a `503` sets acceptance to `refuse` and
   leaves health as it was; any other final response makes it `Alive` and
   keeps acceptance. Missed probes (`HEALTH_THRESHOLD`) and the departed
   address tombstone (ADR-0031 D4) make it `Dead`. The proxy's `Draining`
   health, its grace, and the `503` Reason-text split are gone.

   A turn of a worker's health to `Dead` from another health is a **dead
   verdict**. The proxy tells it to every other `Alive` worker on an
   out-of-dialog OPTIONS carrying the notice, signed with the cluster key the
   workers share (ADR-0055 §12) over the ordinal and the verdict's instant:

   ```
   X-Worker-Dead: worker-0; at=1792155573698; sig=<base64url>
   ```

   A departure is no verdict: the ordinal leaves the projection and its
   address is tombstoned. A worker takes a notice only when it verifies and
   its instant stands within the freshness window of the worker's own clock,
   either way (`B2BUA_DEAD_NOTICE_FRESH_MS`, 10 s); it refuses and counts any
   other (`b2bua_dead_notices_refused_total`). A taken verdict is held for
   `B2BUA_DEAD_VERDICT_HOLD_MS` (42 s), at least the window plus Timer F, the
   last a repeat of its notice arrives, so a repeat is the same verdict; a
   boot refuses a shorter hold, naming both variables. A proxy
   with no cluster key tells nothing and a worker with none trusts nothing.
   A worker takes each verdict once, however many copies reach it, and runs
   on it the takeover half of the departure sweep over the calls it backs for
   the dead worker (ADR-0047 D1, D2) once its own replication stream from that
   worker is down: the proxy now routes their traffic to it, and the stream
   tells a dead worker from a live one the proxy misread. An undecided call's
   decision is asked again there (ADR-0047 D4).
4. **In-dialog routing reads liveness only.** A request carrying a verified
   cookie goes to `w_pri` while `w_pri` is `Alive`, whatever its acceptance;
   otherwise to `w_bak` while `w_bak` is `Alive`. It never runs a fresh
   selection. An `Unknown` worker (just joined, re-hosted, of a new
   incarnation, or before a proxy's first probe answer) is not routed. A
   worker that refuses new calls keeps every dialog it owns, and no takeover
   is caused by its acceptance. A CANCEL goes where its INVITE went while
   that node is an `Alive` worker its cookie names, and a retransmission
   repeats its first copy's target on the same terms; otherwise each follows
   the cookie.
5. **No answer without a target.** An in-dialog request whose two workers are
   both absent or not `Alive`, or that reaches a proxy before its first
   worker view, is dropped and counted
   (`sip_proxy_no_target_dropped_total{kind}`), never answered `503`: the
   sender's retransmission reaches the next view. A worker that receives a
   copy of a replicated call's initial INVITE (the call's CSeq, and a branch
   naming the call's server transaction, unanswered or answered 2xx in the
   record) takes the call over, whichever worker the cookie names: the copy is
   absorbed by the transaction the record rebuilds, which repeats the 2xx the
   record holds. The worker holding the record serves the call; when the
   primary still lives, the callee may get the dialled INVITE twice under one
   branch. A call whose routing decision the record does not hold is no
   copy's: the decision may be in flight at a primary the proxy misread dead;
   a confirmed verdict, the departure or a reclaim asks it again (ADR-0047 D4). An initial INVITE whose cookie
   names another primary and that copies no such record gets no final
   (`b2bua_foreign_invite_copies_dropped_total`; the transaction layer's 100
   Trying has already gone out): the primary still owns that transaction, and
   a final from this worker would end the caller's.
   Accepted residuals:
   - a copy of an undecided call whose primary really died is dropped: the
     confirmed dead verdict (§3) takes the call over and asks its decision
     again;
   - an INVITE that never replicated, from a primary that really died, gets
     no final from the backup; the caller's transaction timers end it;
   - a discovered pool whose watch never delivers a listing never has a first
     view, so every request is dropped: an initial INVITE ends on the caller's
     Timer B instead of a fast `503`; the drop counter and `/readyz` show it.
6. **New calls.** A new dialog goes to an `Alive` accepting worker. When none
   accepts, it goes to an `Alive` refusing one (the fallback of ADR-0043 §12),
   which answers it with its own refusal and writes its call record. The proxy
   authors a refusal of a new call only from its own overload gates (the
   self-gate and the per-worker admission cap, ADR-0009 X6), never from a
   worker's state. The stale-payload sweep caps only a worker whose payload is
   stale while it is `Alive`: a `200` always carries one, so a refusing worker
   is never floored by it.
7. **The cookie's `w_bak`** is the rendezvous winner among `Alive` accepting
   workers other than the primary, and among all `Alive` ones when none
   accepts. A worker leaving the service is not chosen as the backup of new
   calls, and a cluster that refuses everywhere still replicates. The worker
   replicates to the cookie's `w_bak`, unchanged.
8. **No fresh-pod guard.** The guard, its 20 s window and its promotion
   reason are removed. A worker joins `Unknown` (ADR-0012 D4) and is routed
   once a probe answers `200`, which its Ready gate allows only after its
   partition is bootstrapped and current; the calls its backup changed while
   it was away are folded by on-demand reclaim, stand-down (ADR-0047 D7) and
   the version vector (ADR-0014). The proxy resets a worker to `Unknown`
   when its `X-Incarnation` changes, so a restart in place at the same address
   is never routed on the previous process's health. The cookie format is
   unchanged.
9. **The worker's states.**
   - **NotReady**: not yet bootstrapped; OPTIONS `503`.
   - **Ready**: accepting, unless a gating backend's breaker is open, which
     reports `new=refuse; cause=backend` and refuses new calls (ADR-0043 §10).
     Never latched.
   - **Cordoned**: `new=refuse; cause=cordoned`. New calls that reach it are
     refused (bare `503`, its call record written); every established dialog
     is served normally. Reversible, no timer of its own: the operator's next
     step uncordons it, sends SIGTERM, or releases its calls (10). The cordon
     is held by the process: a restart clears it.
   - **Draining**: SIGTERM or withdrawal (ADR-0031 D6); terminal;
     `new=refuse; cause=draining`; the handover drain of ADR-0031 D2,
     unchanged. A cordoned worker that receives SIGTERM drains the same way.
10. **Operator routes** on the worker's probe server, exposed like `/metrics`
    and never on a SIP face:
    - `POST /admin/cordon`, `DELETE /admin/cordon`; `GET /admin/cordon` returns
      the state and the live call count. Gauge `b2bua_cordoned`.
    - `POST /admin/release-all` releases every call the worker owns without
      signalling: each established call's record is closed (answered, ended
      now, cause `operator_release`), its admission holds freed and its replica
      deleted on the backup, and its dialogs dropped; no BYE is sent and no
      backend is consulted. A 2xx still owed its ACK is ACKed, and a leg
      still ringing (a transfer target) is CANCELled, as in setup. A later in-dialog request for it is answered
      `481`. A call still in setup is aborted as usual: `503` to the caller,
      CANCEL to the callee. Only an explicit call releases; neither a cordon
      nor a SIGTERM does.
11. **The proxy's readiness.** `/readyz` is Ready when SIGTERM has not
    latched, no recv shard is stalled, and at least one worker is `Alive`;
    acceptance plays no part, so a backend outage across the cluster never
    turns the proxy NotReady. A starting proxy probes every listed worker as
    soon as it binds and is NotReady until its first worker view (the first
    registry snapshot and one probe round). The VRRP check reads `/healthz`
    and that first-view latch through a route of its own, so a node takes or
    keeps the virtual address only once it can route; the livenessProbe keeps
    reading `/healthz` alone. Equal priorities and preempt stay (ADR-0046 D4).

### Amended

- ADR-0043 §11: the open breaker sets `new=refuse; cause=backend` on a `200`
  instead of a `503`. §12: the fallback stands, and "a node not-ready on a
  backend still serves calls" now holds for in-dialog requests (4).
- ADR-0031 D4: the proxy's `Draining` grace is gone; a leaving worker's
  in-dialog requests move when its ordinal leaves the projection or the probe
  reads it `Dead`.
- ADR-0046 §2: readiness counts `Alive` workers, whatever their acceptance,
  and waits for the first worker view; D4: the VRRP check also reads the
  first-view latch.
- ADR-0012 D4, ADR-0014 §5, ADR-0009: an address change resets health to
  `Unknown` and re-arms nothing; affinity is the ACK/CANCEL exemption and
  liveness routing (4); the drain grace and the fresh-pod guard are removed.

## Consequences

- A backend outage refuses new calls at the workers, each with its call
  record, and touches no established call.
- One worker cut from its backend loses no call: new calls go to the others,
  its own dialogs stay with it.
- A restarted worker's new calls stay on it; a restarted proxy diverts
  nothing once its first probe round has answered.
- A site is taken out of service by cordoning its workers, waiting on their
  call counts, then SIGTERM (calls handed to backups elsewhere) or
  release-all (no backup outside the site). Nothing places a call's backup
  outside its site today (ADR-0015 is unimplemented).
- An in-dialog request with no live worker gets no answer; the sender's
  transaction timers end it.
- Pinned by proxy tests (in-dialog to a refusing primary, to a `NotReady`
  backup, with no target; a `w_bak` among accepting workers; an incarnation
  change resets health; no guard after a join or a proxy restart; no `503`
  before the first view), worker tests (the OPTIONS answer in each state,
  cordon reversal, release-all), and failover-harness cells (a cluster-wide
  backend outage with established calls ended by their BYE; a re-INVITE right
  after a primary returns, served by the primary).
