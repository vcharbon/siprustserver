# Call model vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Call model vocabulary".

**Attempt** (ADR-0042, `Call.attempts`):
One routing answer that dials and the destination legs it dials: the initial
route (dialled by the core or by the service its routing deferred to), a
setup failover, an established call's reroute, a transfer target. Each has a
**cause**, the decision ordinal it opened under, its legs, the ring deadline
its answer states, and once ended an **outcome**: answered, failed (every leg
ended), superseded (the next attempt opened while its legs were still being
cancelled), call ended, or refused (admission refused its route; it dialled
nothing). An attempt with a leg still ringing stays open beside the next. A
dialled leg names its attempt; a media leg names none.
_Avoid_: "attempt" for a retransmission or a re-send (a **rung** of a
**ladder**); "try"; "route" alone for the attempt (the route is its answer).

**Ring deadline** (`no_answer_ms`, `TimerType::NoAnswer`, leg scope):
How long a dialled leg may ring unanswered: its attempt's statement, run
from the first 180–189 the leg's INVITE draws, to the millisecond, and held
inside that INVITE's transaction bound. A leg answered or failed before any
18x never arms it; a later 18x, on any of its early dialogs, does not move it.
A call restored on another node that shows no 18x on an unanswered leg runs
the deadline from its INVITE inside the core, the leg's ring state staying
`Ring::Silent`: the turn that took its first 18x may not have reached that
node, and a service reads the leg as not having rung. An 18x it then takes
re-arms the deadline from that 18x.
_Avoid_: "no-answer timeout" for the time since the INVITE (that is the
**setup** deadline's, or a service's own watchdog).

**Timer scope** (ADR-0042, `TimerScope`):
What a timer guards, and so when it dies: the call, an attempt, a leg, a
retransmission obligation, or the call's setup (`Setup`, the **setup**
deadline's, over while any leg holds a confirmed dialog, a media server's
included, and once the call is terminating). Leaving
the scope retires every timer of it, with no rule cancelling one. A timer is
named by its type and its scope (`TimerKey`) and armed under the call's next
**generation**; a fire the ledger does not hold under that generation, or
whose scope ended whatever the ledger holds, is stale and reaches no rule, so
no rule re-checks for a stale ledger (ADR-0055 §10).
_Avoid_: "timer id" (there is no string name); "cancel" for what a scope's
end does (it **retires** the scope).

**Terminating timeout** (`TimerType::TerminatingTimeout`, `TERMINATING_TIMEOUT_MS`):
The call-scope backstop armed when the call enters `Terminating`, and again on
each BYE it sends while terminating: 32 s, SIP Timer H/J, past which no
legitimate BYE or 2xx retransmission lands. When it fires, every leg still
unresolved is forced terminal, so the call reaches `Terminated` and is removed
with its CDR and its replication delete.
_Avoid_: "safety timer" alone (the call reaper's sweep is another net).

**A-leg** / **B-leg**:
The leg facing the caller (`LegKind::A`, leg id `a`), which this B2BUA answers
as UAS, and a leg toward a called party (`LegKind::Destination`), which it
dials as UAC.
_Avoid_: "inbound leg" / "outbound leg" (they collide with transport
direction).

**Leg kind** (`call::LegKind`, read via `call::helpers::leg_kind`):
The role of a leg: `a`, `destination`, `media` or `transfer-target` (the
leg a REFER dials). A leg with no stated kind is `a` when its id is `a`, else
`destination`.

**Media leg** (`LegKind::Media`):
A leg this B2BUA dials as UAC toward a media server (an MRF) to broker SDP and
exchange control bodies, such as MSCML over INFO. It names no attempt and is
**unadopted**, so the generic relay rules never reach it; it is reaped like
any leg.
_Avoid_: "MRF leg" for the role (MRF names the server).

**Adopted leg** / **Unadopted leg** (`Leg.adopted`, `call::helpers::is_adopted`):
Who drives a leg's in-dialog signalling. The generic relay, keepalive and
failover rules own an adopted leg; only the service that created an unadopted
leg reaches it, through explicit leg actions. A media leg and a transfer
target not yet realigned are unadopted unless the flag says otherwise; every
other leg is adopted. Adoption is not peering: a destination leg is adopted
from its creation, before any merge.

**Active peer** (`Call.active_peer`, `call::helpers::peering`):
The one pair of legs the call bridges at an instant, or none. Merge and split
re-point it across the call's legs; it is never a set, so two pairs are never
bridged at once.
_Avoid_: "peers" for the leg collection.
