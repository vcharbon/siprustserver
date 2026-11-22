# A slow worker is not dead

**Status:** accepted.

**Owner:** the probe's verdicts in `crates/sip-proxy/src/health/probe.rs` and
`health/probe/slow.rs`, the view in `registry/projection/annotations.rs`, the supervision answer
in `core/request/supervision.rs`, the intake ramp in `self_gate/ramp.rs` and
`crates/sip-proxy-runner/src/intake_ramp.rs`.

Amends ADR-0050 §3 to §6 and §11, and ADR-0056 §3.

## Context

The proxy read a worker dead after `HEALTH_THRESHOLD` probes with no answer inside
`HEALTH_TIMEOUT_MS`. A worker whose host is starved of CPU still serves its calls and still
replicates, but answers its probes a little late. Read dead, its calls are taken over by a
backup that then finds its primary alive: the verdict is refuted, or two copies act on one call.
A supervisor in front of the POP sends OPTIONS to the proxy's own address to decide whether the
POP serves. The proxy relayed them to a worker as new requests, so the answer depended on one
worker's load and spent a token of the gates that exist to protect the workers. When every
worker comes back after an outage the supervisor returns the whole POP's traffic at once.

## Decision

1. **The late-answer stamp.** A probe entry is kept `HEALTH_SLOW_WINDOW_MS` (2000) after its
   send. An answer after the reply window and inside that one is a late answer: it stamps the
   worker as answering late, counts nothing else, resets no miss and revives no worker.
2. **Four states.** Health is `Unknown | Alive | Slow | Dead`.
   - `Alive` or `Unknown` turn `Slow` at `HEALTH_THRESHOLD` misses while a late answer is
     within the slow window; `Dead` at that count with none.
   - `Slow` turns `Alive` after `HEALTH_RECOVER_PROBES` (3) answers in time in a row, and
     `Dead` at a miss with no late answer within the slow window, or once
     `HEALTH_SLOW_MAX_MS` (20000) was spent `Slow`, counted over its `Slow` spells until an
     `Alive` spell as long.
   - `Dead` turns `Alive` on one answer in time.
   - A `Slow` or `Alive` worker's endpoint read not ready while its stamp is fresh reads
     `Slow`, not a dead verdict, until the endpoint reads ready again or the probe reads it
     dead.
3. **A slow worker serves its dialogs.** Its cookie routes to it as primary or as backup, and
   the dead notice reaches it. It gets no new dialog, except an emergency one while no worker
   is `Alive`. A turn from `Alive` or `Slow` to `Dead` is a dead verdict; a turn from `Slow`
   to `Alive` tells the worker every standing verdict again.
4. **Readiness and fitness.** The proxy is ready and fit while a worker is `Alive` or `Slow`.
   With every worker `Slow` it answers a normal new call `503` from its gates, places an
   emergency one, and its node keeps the VIPs.
5. **The supervision answer.** An out-of-dialog OPTIONS with no To-tag whose Request-URI names
   the proxy with no user part, at either face's address and port, with no parameter but
   `transport`, is the proxy's own (RFC 3261 §11). While a worker is `Alive` or `Slow`, once
   the first view is in, it is answered `200` with a To-tag and the `Allow`, `Supported` and
   `Accept` a worker last stated on a probe answer. Otherwise it is dropped unanswered and
   counted. It meets no gate and spends no token. `PROXY_LOCAL_OPTIONS=off` relays every
   OPTIONS.
6. **The intake ramp.** The proxy turning fit after an unfit spell that read no worker `Alive`
   or `Slow` starts a ramp: the self-gate admits `PROXY_RAMP_START_PCT` (20) percent of its rate
   from a bucket scaled alike, rising linearly to the whole over `PROXY_RAMP_MS` (30000). The
   process start, an isolation that ended before any verdict, a stalled shard and the external
   face start none: a peer proxy carried the POP meanwhile.
7. **The per-worker cap restarts.** A worker turning `Alive` from `Dead` or `Slow` restarts its
   AIMD cap at `PROXY_RAMP_START_PCT` percent of `LB_CAP_INITIAL_CPS`, the floor at least.
8. **Bounds.** The slow window is at least the reply window and lengthens no verdict on silence:
   `HEALTH_SLOW_WINDOW_MS` + `HEALTH_INTERVAL_MS` ≤ `HEALTH_THRESHOLD` × `HEALTH_INTERVAL_MS` +
   `HEALTH_TIMEOUT_MS`, so a silent worker reads dead within ADR-0050's bound. The proxy refuses
   a set that breaks it at startup, naming every variable. `HEALTH_SLOW_MAX_MS` exceeds the
   worker's `B2BUA_LOOP_EXIT_MS`, checked at render: a wedged worker exits before it is read
   dead for being slow too long.

## Consequences

- A starved worker keeps its calls; nothing is taken over while its answers arrive.
- A worker whose answers stop reads dead as fast as before; one answering past the slow window
  reads dead as a silent one does.
- A one-way partition that drops the answers reads dead; one that delays them reads slow.
- The supervisor's verdict on a POP follows whether any worker serves, not one worker's load.
- A POP back from a total outage takes its traffic over `PROXY_RAMP_MS`.

## Rejected

- **A longer `HEALTH_TIMEOUT_MS`.** It slows every dead verdict, the one a crash needs fast.
- **Answering the supervision OPTIONS `503` with no worker.** A peer proxy's node withdrawing
  its route and the supervisor's own timeout take the POP out. A proxy answering `503` would
  differ from one whose node is already out.
- **Ramping at every fit turn.** A spell that took one proxy out left its peer carrying the
  POP, so its traffic does not come back at once.

## Residuals

- An isolation that outlasts the dead verdict reads no worker serving, so it ramps on its end.
  The proxy cannot tell its own partition from every worker failing.
