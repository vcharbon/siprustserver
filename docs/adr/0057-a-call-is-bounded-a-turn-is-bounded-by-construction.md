# A call is bounded by caps; a turn is bounded by construction; a stuck poll ends the process

**Status:** accepted

**Owner:** `crates/b2bua-core` (fuel, caps, `Tripped`), `crates/b2bua` (the tripped turn's
handling), the runner kits (the poll watchdog), the ADR-0051 site scanner; the full list,
the cap tables and each section's long form are in the
[appendix](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md).

Amends ADR-0020 X6 (a trip is a strike; the reaper's verdict turns run with the collection caps
off; strikes are pruned at the call's release) and ADR-0051 (two more site sets in the ratchet).

## Context

Nothing bounds what one call costs when the decision service or a service misbehaves: a
service can dial until the setup deadline, reset the keepalive window, and grow the record's
collections without a cap. Inside the process, a synchronous loop that never exits holds its
thread for ever, and a stack overflow aborts the process. The goal is survival: a node receiving
one pathological call per second keeps serving the others. A panic in a turn already costs one
call (ADR-0020 X6); this ADR brings every other runaway to that cost, or prevents it.
Detail: [appendix](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#context).

## Decision

### 1. A cap is a failsafe

A cap sits well above any valid call (§9). It can be raised, never switched off. A **defect
cap** is reached only by a defect, a **peer cap** by a peer's traffic (§2). A `Keepalive` is
armed exactly one interval out, and `Rung`, `NotifyAnswerCap` and `HeldAnswer` are
framework-only timer types.
Detail: [appendix §1](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#1-a-cap-is-a-failsafe).

### 2. The call record's collections are capped at the insert

`b2bua-core` checks each collection's length when an action would grow it: `B2BUA_CAP_LEGS`,
`_ATTEMPTS`, `_TIMERS`, `_REQUESTS`, `_CDR_EVENTS`, `_DIALOGS`, `_HEADER_UPDATES`, `_EXT_BYTES`.
The caps are collection lengths, not new counters. A service's HTTP timeout is clamped to
`B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS`.
Detail: [appendix §2](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#2-the-call-records-collections-are-capped-at-the-insert).

### 3. A turn carries fuel

`b2bua-core` counts the actions one turn returns, rule and lifecycle actions together, against
`B2BUA_CAP_ACTIONS_PER_TURN`. The count lives in the turn's scope and is reset every turn, so a takeover copy re-running the event counts the same (ADR-0052).

### 4. A trip fails the turn, as a panic does

A cap or the fuel crossed returns `Tripped { cap }`. The shell commits nothing, writes the
safepoint owing the limiter release, answers an in-dialog request in hand `500`, and reports the
turn through the failure hook. The `fatal-error` verdict then tears the call down from its
safepoint (a bare `503` during setup). Verdict turns and turns of a terminating call run with
the collection caps off. The cap's name never reaches the wire.
Detail: [appendix §4](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#4-a-trip-fails-the-turn-as-a-panic-does).

### 5. Loops and recursion are bounded by construction

The ADR-0051 ratchet gains two site sets, counted by `tooling/site-scan`: **loop sites** in the
pure tier and SDK services (a loop is a `for` over a finite collection or range, or
`bounds::bounded`), and **recursion sites** in every tiered crate (allowed only behind
`bounds::Depth`). In `sip-message` a loop over the input is a `for` or the bounded cursor.
Detail: [appendix §5](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#5-loops-and-recursion-are-bounded-by-construction).

### 6. A poll stuck on the CPU ends the process

A watchdog thread reads each polling thread's slot and CPU time (`schedstat`). A poll is
**stuck** when it stayed in one poll while its thread spent more than `B2BUA_STUCK_POLL_CPU_MS`
(10 000) of CPU; wall time is never read. A stuck call body's call is discharged (its parties
left connected), then the process exits, with a hard `std::process::exit` as backstop. The
proxy carries the same watchdog (`PROXY_STUCK_POLL_CPU_MS`).
Detail: [appendix §6](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#6-a-poll-stuck-on-the-cpu-ends-the-process).

### 7. Detection

`b2bua_cap_trips_total{cap, kind}`, `b2bua_cap_near_total{cap}`, a `WaveSet` log per cap, the
turn's wall time and allocated bytes, the watchdog's log line, and `b2bua_cap_peak{cap}`. A
defect-cap trip is an alert; a peer-cap trip is a counter.
Detail: [appendix §7](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#7-detection).

### 8. Configuration

Each cap is a `B2BUA_CAP_*` variable, on by default. Startup refuses a zero cap and the
relations the appendix lists between attempts, legs, the HTTP timeout and the fuel.
Detail: [appendix §8](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#8-configuration).

### 9. Defaults are a measured maximum times a factor set by what the cap costs

Each default is the largest value measured over the replay corpus and a soak, times a factor:
500 and more for the fuel, 1 000 for attempts, 100 for legs, 20 for memory a call holds. A cap
on what grows with age is a base plus a rate per granted hour.
Detail: [appendix §9](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#9-defaults-are-a-measured-maximum-times-a-factor-set-by-what-the-cap-costs).

## Consequences

- A looping decision service or service defect costs a bounded number of legs, attempts and
  actions per call, and the call ends as a panicked one does.
- No new replicated counter.
- A carrier that forks without bound, or a peer that floods relays, ends its own call and
  raises a counter, not an alert.
- A stuck poll in a call body costs the node and leaves that one call's parties connected.
- The ratchet's new sets land crate by crate, as ADR-0051's do.

## Considered and deferred

- A pool for the core fold, with a leaked thread at `SCHED_IDLE`: built when §6 records a
  stuck poll. [Detail](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#considered-and-deferred).

## Considered and rejected

- Discharging a tripped call with nothing on the wire: parties keep talking, unbilled.
- Refusing a service's `Keepalive` arm: the clamp bounds its rate instead.
- Cancelling a turn by timeout: a cancel at an await leaves effects half applied.
- A wall-time trip: a trip must be deterministic, and the same on a takeover copy.
- Killing or recovering a stuck thread: undefined behaviour through Rust frames.
- A cooperative cancel flag in the bounded helpers: it only shortens a finite computation.
- A process or WebAssembly sandbox per turn: a copy of the record every turn.
- A memo of the bytes of a request that stuck a poll: §5 and fuzzing prevent that request.
- An encoded-size check of the record: it runs after the turn landed.
- Replicated per-call counters: they need merge rules; lengths need none.
- A Q.850 cause, or the cap's name, on the trip's final.
- A count-only mode: it protects nothing.

[Detail](appendix/0057-a-call-is-bounded-a-turn-is-bounded-by-construction.md#considered-and-rejected).
