# Call reaper vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Call reaper vocabulary".

The terminal-guarantee module (design in progress; extends ADR-0010 X5's
invariant enforcer). Its one-sentence interface: **every call admitted to the
live map is released exactly once, through the existing `release_call` +
invariant path, with exactly one CDR**, guaranteed in-process (panic, dropped
event, lost timer, wedged `Terminating`); a hard-crash window is out of scope.

**Call reaper**:
The module owning that promise. Three implementation legs, all funnelling into
the *existing* teardown path (never a second teardown vocabulary): a handler
panic becomes a synthetic event through the normal rules (two-strike: a second
panic on the same call discharges **obligations** directly and releases); a
liveness sweep keyed off the **last-touched stamp** catches wedged calls; both
ride the per-call FIFO + lock.
_Avoid_: "garbage collector", "orphan sweep" (the source-era concept); reaping
`bak:` **Elements** or **takeover copies** (**self-release** owns those, push-based
on `CallQuiesced`, ADR-0014).

**Obligation**:
A consequence of admitting a call that must be discharged exactly once at
release, **derivable from the persisted `Call` snapshot alone** and idempotently
dischargeable through the normal effects pipeline. The two: the owed CDR
(from the call itself) and one limiter release for a call that sent an admit
request, by the key `limiter` carries (`CallLimiterState::owed_release`).
The Call *is* the ledger.
_Avoid_: a parallel allocation registry / resource ledger (the mirror/slice
divergence hazard; would not survive failover, while `limiter` and
`cdr_events` ride the replicated Element for free).

**Release queue**:
The worker's queue of limiter releases. The **obligation**'s limiter
release is handed to it and the call ends in its last turn; the queue sends
every waiting key in one request, backs off after a failed send, and gives
up an entry that waited one lease or is the oldest at its cap (ADR-0040). It
holds keys only and is not replicated: the lease covers a crashed worker.

**Last-touched stamp**:
The node-local, store-side per-call activity timestamp, refreshed on every
dispatched event and at hydration/**reclaim**, that is the reaper's *only*
liveness input. Never `created_at`, never timer deadlines (keepalive catch-up
smoothing makes a freshly reclaimed call look overdue).

**Acting-backup terminal contract**:
An acting backup terminates a call (CDR + cleanup + propagated delete) only
when *serving an explicit termination message* (BYE/CANCEL) during its takeover
window. Timer-driven teardown (GlobalDuration, keepalive timeout) of a
failed-over call waits for the **reclaiming** primary's reaper. The reaper
sweep runs only where the node serves the call live as primary.
