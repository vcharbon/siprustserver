# A call is bounded by caps; a turn is bounded by construction; a stuck poll ends the process

**Status:** proposed (2026-11-08)

**Owner:** `crates/b2bua-core` (the turn's fuel, the caps checked at insert, the
`Tripped` result, the framework-only timer types), `crates/call` (the keepalive
clamp), `crates/b2bua` (the shell's handling of a tripped turn, the failure hook,
the task-to-call map), `crates/b2bua-sdk` (the cap settings),
`crates/b2bua-runner-kit` and `crates/sip-proxy-runner` (the poll watchdog and its
exit), `crates/sip-message` (the bounded cursor), the site scanner of the ADR-0051
ratchet (the loop and recursion sets).

Amends ADR-0020 X6 (a trip is a strike; the reaper's verdict turns run with the
collection caps off; strikes are pruned at the call's release) and ADR-0051 (two
more site sets in the ratchet).

## Context

Nothing bounds what one call costs when the decision service or a service built
on the B2BUA misbehaves. A decision service that answers every failure with
another route dials until the setup deadline; a route that fails at once costs
no timer, so its pace is only the decision round trip. The message caps (per
keepalive window, and per lifetime at dispatch) are the only per-call bounds; a
service can reset the window by arming the keepalive timer early, and can loop on
a timer type neither cap counts. The call record's collections grow without a cap
of their own: the legs and attempts a call dials over its life (a release reroute
on an answered call has no count), the timer ledger, the request records, the CDR
events, the dialogs of a leg, a service's ext value. Nothing counts the actions
one turn returns.

Inside the process, Rust cannot preempt a thread and tokio cancels only at an
await. A synchronous loop that never exits holds its worker thread for ever. A
stack overflow is no panic: the guard page aborts the process, with no unwind and
no `JoinError`. One unbounded recursion in the parser, on a request a peer
retransmits and an upstream element moves to its next target, takes down every
node the request reaches.

The goal is survival: a node receiving one pathological call per second, from a
defect no test found, keeps serving the others. A panic in a turn already costs
one call (ADR-0020 X6); this ADR brings every other runaway to that cost, or
prevents it.

## Decision

### 1. A cap is a failsafe

A cap sits well above any valid call (§9). No cap shapes valid traffic, and no
cap can be switched off: it can be raised. A cap only a defect reaches (in a
service, the decision service or the B2BUA) is a **defect cap**; a cap a peer's
traffic can reach (§2) is a **peer cap**. Both end the call the same way; they
differ in what is alerted (§7).

The message caps are the peer caps that bound a peer's rate. They keep their own
handling (ADR-0020): the per-window cap answers an unanswered caller `503` and
tears the call down on the wire, the lifetime cap sends its `message-cap`
verdict. The lifetime cap holds with the reaper's sweep on or off. Two timer
rules close the window's escapes:

- a `Keepalive` is armed exactly one keepalive interval out, whoever arms it and
  whatever it asks for, so the window resets at most once per interval;
- `Rung`, `NotifyAnswerCap` and `HeldAnswer`, which neither cap counts, are
  framework-only types: a rule's arm of one is dropped as a defect, as an
  undeclared lifecycle action is.

### 2. The call record's collections are capped at the insert

`b2bua-core` checks each collection's length when an action would grow it:

| Cap | Collection | Kind | Bounds |
|---|---|---|---|
| `B2BUA_CAP_LEGS` | `b_legs` (never pruned) | peer | legs a call dials over its life, for every cause (REFER and 3xx chains included), media legs included |
| `B2BUA_CAP_ATTEMPTS` | `attempts` | defect | routes tried, refused admissions included (ADR-0042) |
| `B2BUA_CAP_TIMERS` | the timer ledger | defect | timer keys held at once, service-minted keys included |
| `B2BUA_CAP_REQUESTS` | request records | peer | requests in flight, relays and service HTTP included |
| `B2BUA_CAP_CDR_EVENTS` | `cdr_events` | peer | |
| `B2BUA_CAP_DIALOGS` | the dialogs of one leg | peer | forks, late fork 2xx |
| `B2BUA_CAP_HEADER_UPDATES` | stored header updates | defect | |
| `B2BUA_CAP_EXT_BYTES` | one service's ext value, encoded | defect | |

The caps are collection lengths, not new counters: the record already replicates
and merges the collections, and the check costs the same with or without a
backup. An undiverged takeover copy reads the same lengths and trips the same
way; a union merge after a divergence may exceed a cap neither copy reached, and
the merged call is torn down anyway. A route that fails at once is one attempt
like any other; no wait is added between attempts. A consult that grants no leg
(a refused REFER, a release consult) is bounded by the message caps.

A service's HTTP request carries a timeout no longer than
`B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS`; a longer one is clamped.

The allocator's bytes per turn are reported (§7), never a trip. What the caps do
not bound, the process's memory as a whole, admission bounds (ADR-0037).

### 3. A turn carries fuel

`b2bua-core` counts the actions one turn returns, rule actions and lifecycle
actions together, against `B2BUA_CAP_ACTIONS_PER_TURN`. The count lives in the
turn's scope and is reset every turn: the turn is a function of its inputs
(ADR-0052), so a takeover copy re-running the event counts the same.

### 4. A trip fails the turn, as a panic does

A cap or the fuel crossed makes the core return `Tripped { cap }` in place of the
turn's result. The shell discards the result: nothing of the turn is committed,
so the store holds the call as it was before the event, its last safepoint (the
record of an initial INVITE is written before its first fold). The fired timer
the turn consumed stays in the safepoint's ledger with no driver timer behind it,
and the teardown's retirement of the call's timers clears it. The shell then:

- writes the safepoint owing the release of the call's limiter key (ADR-0040), so
  the termination releases every hold the call took, an initial turn's admission
  or an off-turn fold's alike;
- answers an in-dialog request in hand, when it holds a server transaction the
  turn would have answered, with the stack's bare `500` built from the event. The
  caller's initial INVITE is not answered here: the verdict's teardown answers it
  (ADR-0022);
- reports the turn through the dispatcher's failure hook, as a panic is reported,
  labelled with the cap. The hook sends the verdict whether or not the reaper's
  sweep is on, and a call's strikes are pruned at its release.

The `fatal-error` verdict then ends the call from its safepoint through the
normal rules, as it ends a panicked call: `teardown_on_the_wire` sends a BYE to
each confirmed leg, a CANCEL to each early leg and the bare `503` an unanswered
caller is owed, with `TerminatingTimeout` bounding the wait. The CDR, the limiter
release and the delete, the backup's copy included, run through the one
enforcement path. The reaper's verdict turns run with the collection caps off
(the fuel stays): a safepoint at a cap would otherwise trip again on the
teardown's own BYE record, CANCEL record, timer or CDR event, and the second
strike's discharge would leave the parties connected. So does every turn on a
call already terminating, for the same reason: the answers to its teardown and
its `TerminatingTimeout` would trip, and the call would never end. What a
peer sends in that window is bounded by the message caps and that timeout. A verdict turn that fails
for any other reason is the second strike: the discharge forces the call terminal
with no rule run (ADR-0020 X6).

During setup, before an INVITE left for a callee, the call ends with the caller's
bare `503` and nothing else: a bare `5xx` is the failure an upstream element
moves to its next target on (RFC 3261 §16.7), and each node the call reaches
pays one cap at most. The cap's name reaches the counters, the logs and the CDR,
never the wire.

### 5. Loops and recursion are bounded by construction

The ADR-0051 ratchet gains two site sets, counted by a scanner over the crates'
syntax (`tooling/site-scan`, stable toolchain; clippy counts neither) in the
same allowlist as the panic set:

- **loop sites**, in the pure tier and in services built on the SDK: `loop`,
  `while`, each `Iterator::next` or `next_back` written by hand, the
  constructors of infinite iterators (`iter::repeat`, `repeat_with`,
  `successors`, `from_fn`, `cycle`), and an iterated range (or `iter::repeat_n`)
  whose end is neither a constant nor a collection's `len()`, an immutable
  local bound to one of those included and a wide integer type's `MAX`
  excluded. A loop is a `for` over a finite collection or such a range, or the
  helper `bounds::bounded(max, …)`, which charges one unit per iteration and
  returns an error past `max`; the `bounds` crate holding it is outside the
  set. A `loop` or `while` the compiler evaluates, in the initializer of a
  `const` or `static` item or an inline `const` block, is none: its
  evaluation limit ends it; a closure, `async` block or item inside one runs
  as other code does and is read as such. With §2's caps, a turn's work is
  bounded. The shells' event loops (`loop { select! }`) belong to no call
  and are not loop sites;
- **recursion sites**, in every tiered crate: a cycle in a crate's call graph,
  direct or mutual, resolved by path: a path naming a function of the crate
  (`Trait::f` and `<T as Trait>::f` included), called or passed as a value,
  and a method called on `self` are its edges, a call through a trait reaching
  its default and every implementor's own. A recursion is allowed only behind
  a depth guard, `bounds::Depth`, which returns an error past its maximum
  depth or when `stacker::remaining_stack()` falls below a margin: a function
  on each cycle takes one and calls `descend` on it. Each guarded site has a
  test at its maximum depth. The rule chain runs one rule per event and no
  rule calls the chain, so `dyn` rule dispatch carries no recursion; other
  calls through `dyn`, function pointers, a method called on another receiver
  and drop glue are outside the scanner and inside review.

A macro's own body is outside the scanner for both sets; its arguments are
read where they parse as expressions.

In `sip-message`, a loop over the input is a `for` over an iterator of it, or
the bounded cursor: at most `len + 1` iterations, each consuming at least one
byte, or a recoverable no-progress error (ADR-0051: no panic). The fuzz campaign
carries a no-progress target.

Bounded is not cheap: nested loops over capped inputs stay finite and may still
be slow. §6 is the backstop for that, and for everything outside the sets.

### 6. A poll stuck on the CPU ends the process

A thread registers with the watchdog on its first `on_before_task_poll` (poll
hooks, under `tokio_unstable`), through a thread-local: only threads that run
polls register, so blocking-pool threads, which tokio also starts its workers
from, never do. Each registered thread keeps a sequence number in its slot, odd
inside a poll and even between polls, and its thread id. A watchdog std thread
reads the slots once a second (twice per threshold below two seconds), and each
registered thread's CPU time
(`/proc/self/task/<tid>/schedstat`). A poll is **stuck** when its slot stayed odd
on the same sequence number while its thread spent more than
`B2BUA_STUCK_POLL_CPU_MS` (default 10 000) of CPU. A thread that is throttled,
starved, stopped or blocked spends no CPU and never reads stuck; driver work
between polls is never charged; wall time is never read. A thread blocked behind
a stuck one is not stuck, and needs not be: the stuck one is. A node whose
`schedstat` cannot be read refuses to boot.

A stuck poll, in a call body or anywhere else, logs the task, the call when it is
a call body (the dispatcher maps each body's task id to its call), and the CPU it
spent. When it is a call body, the exit then writes the call's discharge
(`discharge_result`: no rule code, the CDR and the limiter release, the backup's
copy deleted), best effort within the shutdown's flush bound: the input the
record carries (a fired timer, an undecided decision, a retransmitted request) is
not re-run by the backup's takeover or by the node's own reclaim after its
restart. The parties of that one call are left connected. The process then
leaves through the worker's shutdown path; the watchdog thread arms a hard
`std::process::exit` past the shutdown's flush bounds plus one second, so the
exit completes even when the only worker is the stuck one. The discharge first
fences the stuck body off: a turn of it that goes on, slow rather than stuck,
stops at its next fence check (before its write, its CDR, its sends and its
release), and the call's worker runs no further item of it; a body that is no
longer the call's in-flight one ended its turn, and its call is left to the
takeover, as it is once the CDR sink's close has begun or the node is
superseded. The discharge owes the release of the call's limiter key whatever
the record says: the stuck turn may have sent an admit. The
runtime's workers keep no LIFO slot: a task the stuck poll woke last would wait
there, unstolen, and the discharge's write would never reach the backup. The
cost is the slot's shortcut: a task one task wakes runs after those already
queued on that worker. HA takes the node's
other calls over (ADR-0047). The front proxy's runner carries the same watchdog
and hard exit (`PROXY_STUCK_POLL_CPU_MS`); it holds no call, and its traffic
moves off it as its route is withdrawn (ADR-0056).

A spin that escapes §5 thus costs one node per delivery of the input that causes
it, and the discharge keeps the record from delivering it again; §5 and fuzzing
are what keep that input from existing.

### 7. Detection

- `b2bua_cap_trips_total{cap, kind}`, and `b2bua_cap_near_total{cap}`, counted on
  each crossing of half a counted collection's cap by a turn that lands (a
  service's ext value is measured once per turn and has no crossing);
- a trip is logged through an `observe::WaveSet` per cap (ADR-0026), the call
  named in its per-call trace, so a decision service that trips every call does
  not make logging scale with traffic;
- the turn's wall time as a histogram, `Instant` around the turn, and the
  allocated bytes of an in-dialog `rule_chain_turn`, jemalloc's
  `thread.allocated` read around that synchronous call on the one thread it runs
  on (the initial turn's folds span awaits and are not measured);
- the watchdog's log line, the last before the exit;
- `b2bua_cap_peak{cap}`, the largest reading of each cap a landed turn left
  (a collection's length, the ext value's bytes, the turn's actions), and
  `b2bua_cap_peak_per_hour{cap}`, an aged cap's reading per started hour of
  the call's grant: the measurement of §9.

A defect-cap trip is an alert; a peer-cap trip is a counter.

### 8. Configuration

Each cap is a `B2BUA_CAP_*` variable with its role, default and relations in the
settings' documentation, on by default. Startup refuses:

- a cap at zero;
- `B2BUA_CAP_ATTEMPTS` below `B2BUA_CAP_LEGS × (MAX_LIMITER_FAILOVER + 1)`, since
  every leg may follow refused admissions;
- `B2BUA_CAP_SERVICE_HTTP_TIMEOUT_MS` above the service answer deadline less its
  margin;
- `B2BUA_CAP_ACTIONS_PER_TURN` at or below `B2BUA_CAP_LEGS` (its base) plus the
  teardown's two further actions: a call at its leg cap before any grant could
  not be torn down. A teardown takes one action per unresolved leg, far fewer
  than the legs a granted call may have dialled.

The upstream defaults are constants stated with their derivation from the
scenario catalogue's heaviest shapes; a deployment sets its own from measurement
(§9).

### 9. Defaults are a measured maximum times a factor set by what the cap costs

Each default is the largest value measured over the replay corpus and a soak
(the maximum over valid calls, never the average) times a factor that follows
what reaching the cap costs the node:

| Caps | Factor | Why |
|---|---|---|
| the fuel | 500 and more | an action costs CPU and the turn's transient effects, and the CPU has orders of magnitude of room; above the teardown of a call at its base leg cap |
| attempts | 1 000 | an attempt record holds tens of bytes |
| legs | 100 | a route count, and the record's heaviest entry: kilobytes each |
| timers, requests, dialogs, CDR events, header updates, ext bytes | 20 | memory a call holds: past twenty times, a few runaway calls weigh on the node |

A cap on what grows with a call's age (legs, attempts, CDR events) is a base
plus a rate per hour of the call's granted maximum duration, read from the call,
each its factor times the largest measured, since captures truncate long calls.
The measurement also states the memory one call holds with every cap reached at
once; past what the sizing formula counts, admission (ADR-0037) refuses new
calls before the node grows further.

## Consequences

- A looping decision service or service defect costs a bounded number of legs,
  attempts and actions per call, before and after answer, and the call ends as a
  panicked one does: torn down on the wire from its safepoint, with its CDR and
  its limiter release.
- No new replicated counter: the caps read collection lengths the record already
  holds and merges.
- A carrier that forks without bound, or a peer that floods relays, ends its own
  call and raises a counter, not an alert.
- A stuck poll in a call body costs the node and leaves that one call's parties
  connected.
- The ratchet's new sets land crate by crate, as ADR-0051's do.

## Considered and deferred

- **A pool for the core fold, a leaked thread at `SCHED_IDLE`.** The pure core
  fold runs on dedicated threads and the body awaits its result; the watchdog
  resolves the body's wait with "stuck", the body takes §4's path, and a late
  result lands in a dropped receiver. A stuck thread is left at `SCHED_IDLE` and
  replaced; the process ends past a leak budget. One stuck call then costs one
  call, not a node. Cost: one handoff between threads per turn, about 3–8 µs of
  CPU (1–3 % of an 8-core node at 20–30k turns/s, estimated), about 12 call sites
  given owned inputs, a call stuck twice dropped by the shell alone; `SCHED_IDLE`
  protects only a process without a CPU quota; the parser and the shell's
  synchronous code stay outside it. It is independent of the rest of this ADR and
  is built when §6 records a stuck poll.

## Considered and rejected

- **Discharging a tripped call with nothing on the wire.** The B2BUA relays no
  media: parties left connected keep talking, unbilled and outside the limiter,
  and a peer able to reach a cap would get a free call.
- **Refusing a service's `Keepalive` arm**: services arm it on answer; the clamp
  bounds its rate without taking dead-peer detection away from their calls.
- **Cancelling a turn by timeout**: tokio cancels at an await, and the turn's
  shell awaits the store and replication; a cancel there leaves effects half
  applied. A synchronous spin never reaches an await at all.
- **A wall-time trip**: box load, cgroup throttling and a debugger all read as
  slow; a trip must be deterministic, and the same on a takeover copy.
- **Killing or recovering a stuck thread**: `pthread_cancel` through Rust frames,
  and unwinding or `longjmp` out of a signal handler, are undefined behaviour.
- **A cooperative cancel flag in the bounded helpers**: the helpers already bound
  their loops; the flag would only shorten a finite computation.
- **A process or a WebAssembly sandbox per turn**: a copy of the record across a
  boundary every turn.
- **A memo of the bytes of a request that stuck a poll**: §5 and the fuzz
  campaign keep such a request from existing; the exit's discharge keeps the
  record from carrying it.
- **An encoded-size check of the record**: it exists only where a replicating
  store is wired, and runs after the turn has landed, where the turn can no
  longer be discarded. The per-collection caps bound the record on every node.
- **Replicated per-call counters**: they need merge rules and diverge on quiet
  turns; collection lengths need neither.
- **A Q.850 cause, or the cap's name, on the trip's final**: the wire learns
  nothing of the caps.
- **A count-only mode**: a cap that only counts protects nothing.
