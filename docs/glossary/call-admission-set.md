# Call admission set vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Call admission set vocabulary".

A call's holds on the call limiter, keyed by the call
([ADR-0040](../../docs/adr/0040-admission-holds-keyed-by-the-call.md)). The call
side is `call::CallLimiterState`, written only through its operations; the
worker side and the call-side recipes live in `b2bua::limiter`.

**Admission set**:
The limiter entries (an id and the cap it is admitted under) a call holds
under its limiter key. Every change replaces the whole set in one admit.
_Avoid_: "reservation", "permit" (the new-call admission ladder owns that).

**Held**:
The set the limiter last stated it holds for the call, under the change
number of that statement. An older statement never replaces it. Every admit
carries it, and every change is computed from it.

**Target**:
The set the call's latest admit asked for; after a refused or superseded
change, the held set. The call runs **fail open** while the target names an
id held lacks.

**Counted**:
Held is not empty: the limiter confirmed a set, and the call refreshes its
lease while Active.

**Change number**:
The number every admit of a call carries, above every number the call sent
before; the limiter refuses one not above the number it knows for the key
(superseded). A call materialised on another node moves to its next epoch.

**Runs uncounted**:
The call runs on an id held lacks: it runs fail open, or on a set a service
or a route moved it onto that held does not name. Restated on every write of
the call, so the next statement that holds the set ends it.
