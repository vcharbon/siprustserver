# A slow worker is not dead

**Status:** accepted.

**Owner:** the probe's verdicts in `crates/sip-proxy/src/health/probe.rs` and
`health/probe/slow.rs`, the view in `registry/projection/annotations.rs`, the supervision answer
in `core/request/supervision.rs`.

Amends ADR-0050 §3 to §6 and §11, and ADR-0056 §3.

## Context

The proxy read a worker dead after `HEALTH_THRESHOLD` probes with no answer inside
`HEALTH_TIMEOUT_MS`. A worker whose host is starved of CPU still serves its calls and still
replicates, but answers its probes a little late. Read dead, its calls are taken over by a
backup that then finds its primary alive: the verdict is refuted, or two copies act on one call.
A supervisor in front of the POP sends OPTIONS to the proxy's own address to decide whether the
POP serves. The proxy relayed them to a worker as new requests, so the answer depended on one
worker's load and spent a token of the gates that exist to protect the workers.

## Decision

1. **The late-answer stamp.** A probe entry is kept `HEALTH_SLOW_WINDOW_MS` (2000) after its
   send. An answer after the reply window (1.6 s) and inside that one is a late answer. It
   stamps the worker as answering late when it would read the worker `Alive` (a `200`, not a
   `503`) and names no other `X-Incarnation` than the last one read; it counts nothing else,
   resets no miss and revives no worker. Any late answer gives the proxy reach.
2. **Four states.** Health is `Unknown | Alive | Slow | Dead`.
   - `Alive` turns `Slow` at `HEALTH_THRESHOLD` misses while a late stamp is within the slow
     window, and so does `Unknown` on an endpoint that reads ready, or in a view where none
     does; `Dead` at that count with none. A worker answering later than the slow window
     reads `Dead`, as a silent one does.
   - `Slow` turns `Alive` after `HEALTH_RECOVER_PROBES` (3) answers in time in a row, and
     `Dead` at a miss with no late stamp within the slow window. A worker answering inside it
     stays `Slow`; a wedged one goes silent and its stalled-loop exit ends it. A `Slow` worker
     whose answer names another `X-Incarnation` reads `Dead` first: its process died.
   - `Dead` turns `Alive` on one answer in time.
   - A `Slow` or `Alive` worker's endpoint read not ready while its stamp is fresh reads
     `Slow`, not a dead verdict, until the endpoint reads ready again, the probe reads it
     dead, or an answer in time names another process.
3. **A slow worker serves its dialogs.** Its cookie routes to it as primary or as backup, a
   request no memo places ranks it with the `Alive` ones, and the dead notice reaches it. It
   gets no new dialog, except an emergency one while no worker is `Alive`. A turn to `Dead` is
   a dead verdict from `Alive` or `Slow`, in any view, and from `Unknown` on an endpoint that
   reads ready; a turn from `Slow` to `Alive` tells the worker every standing verdict again.
4. **Readiness and fitness.** The proxy is ready and fit while a worker is `Alive` or `Slow`.
   With every worker `Slow` it answers a normal new call `503` from its gates, places an
   emergency one, and its node keeps the VIPs.
5. **The supervision answer.** An out-of-dialog OPTIONS with no To-tag whose Request-URI names
   the proxy with no user part, at either face's advertise or specific bound address (an
   unspecified bind names none) and port, with no parameter but `transport`, is the proxy's own
   (RFC 3261 §11). While a worker is `Alive` or `Slow`, once the first view is in, it is
   answered `200` with a To-tag and the `Allow`, `Supported` and `Accept` of the last `200` a
   worker answered a probe with. Otherwise it is dropped unanswered and counted. It meets no gate
   and spends no token. `PROXY_LOCAL_OPTIONS=off` relays every OPTIONS.
6. **Bounds.** The slow window is at least the reply window and lengthens no verdict on silence:
   `HEALTH_SLOW_WINDOW_MS` + `HEALTH_INTERVAL_MS` ≤ `HEALTH_THRESHOLD` × `HEALTH_INTERVAL_MS` +
   `HEALTH_TIMEOUT_MS`. The dead verdict lands at most `HEALTH_THRESHOLD` × `HEALTH_INTERVAL_MS`
   + `HEALTH_TIMEOUT_MS` (3.6 s) after the last answer the proxy received, in time or late;
   after a late one, at most `HEALTH_SLOW_WINDOW_MS` + `HEALTH_INTERVAL_MS` (3 s). The proxy
   refuses a set that breaks the relation at startup, naming every variable.

## Consequences

- A starved worker keeps its calls; nothing is taken over while its answers arrive.
- A worker whose answers stop reads dead as fast as before; one answering past the slow window
  reads dead as a silent one does.
- A one-way partition that drops the answers reads dead; one that delays them reads slow.
- The supervisor's verdict on a POP follows whether any worker serves, not one worker's load.

## Rejected

- **A longer `HEALTH_TIMEOUT_MS`.** It slows every dead verdict, the one a crash needs fast.
- **A cap on the time a worker stays `Slow`.** A worker answering inside the slow window
  serves; a wedged one goes silent and reads dead, and its own stalled-loop exit ends it. A cap
  would take the calls of a live worker over.
- **Answering the supervision OPTIONS `503` with no worker.** A peer proxy's node withdrawing
  its route and the supervisor's own timeout take the POP out. A proxy answering `503` would
  differ from one whose node is already out.
- **An intake ramp after an outage, and a low restart of each returning worker's cap.** The
  workers' admission already sheds an excess with fast `503`s the sender retries elsewhere,
  and telling the proxy's own partition from a worker outage made the trigger fragile.

## Residuals

- Every worker turning late in one round: from the last answer in time, the next one is late by
  the rise in lateness, so the proxy reads isolated for up to one interval plus that rise, less
  `HEALTH_ISOLATION_MS`. A rise above about 1 s on every worker withdraws every proxy of the
  POP, which returns after the first late answer and the route guard's hold.
- A worker alive that answers later than the slow window reads dead: its in-dialog requests go
  to its backup while it still runs, until it exits or answers in time again.
- A POP back in rotation takes its full share at once. The workers' admission sheds any excess
  with fast `503`s, which the sender retries elsewhere.
- A CANCEL that crosses to the proxy holding no memo of its INVITE during a `Slow` spell goes to
  the rank winner among the workers serving dialogs. When that winner is the `Slow` worker the
  INVITE went past, the CANCEL misses the worker holding it, and the call rings on until its own
  timers end it.
- One proxy reading every worker `Slow` while its peer reads them `Alive` stays announced: it
  answers the normal new calls of its flows `503`, which the sender retries elsewhere.
