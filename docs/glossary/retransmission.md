# Retransmission vocabulary

Full definitions of the terms [CONTEXT.md](../../CONTEXT.md) indexes under "Retransmission vocabulary".

**Ladder**:
The whole sequence of scheduled re-sends of one message, and the schedule that
paces it. A ladder belongs either to a **transaction** (Timer A/E/G, the CANCEL
sub-ladder; the transaction layer drives them and its consumers never see
them) or to a **dialog-level obligation** (RFC 3261 §13.3.1.4, RFC 3262 §3;
replicated with the call, visible to rules only as its give-up). See ADR-0032.
_Avoid_: "retry" (it carries HTTP retry semantics elsewhere in the workspace),
"attempt" (a routing answer's dial, see **Attempt**), "cadence" for the
schedule as a whole.

**Rung**:
One step of a ladder. Rung 0 is the original send, rung 1 the first re-send,
rung *n* the *n*th. Since the schedule is fixed by the class, a rung index is
the whole of a ladder's state, with no epoch anchor, so it survives takeover.

**Retained emission**:
The exact serialized datagram of a message that owes repeats, kept opaque and
replicated with the call. A rung re-sends these bytes; it never re-composes an
equivalent message (ADR-0032 X3).

**Obligation**:
What discharges a dialog-level ladder, as a key the framework matches on:
`AckOf2xx` and `AwaitPrack`. The engine cancels the ladder when the matching
ACK or PRACK arrives; a rule sees only a 2xx's give-up. A transaction ladder needs
no obligation; the transaction layer owns its own.
_Avoid_: "watchdog" for the 2xx pair (it names the timer, not the thing
owed).

**Provisional record**:
One reliable provisional (RFC 3262) the call received, written by the engine
before any rule reads it: the responder's identity of it (leg, tag, INVITE
CSeq, `RSeq`), where the PRACK the responder is owed stands (pending, relayed,
this stack's own, deferred to the ACK, waived), and where it was shown under
this stack's own number, with its ladder. A repeat of one, or one out of
`RSeq` order, is dropped before the rules. A rule decides only whether it is
shown; the engine sends the PRACK this stack owes.
_Avoid_: "relayed provisional" for the record (one shown to no one has a
record too); "owed PRACK" for a PRACK owed TO this stack (that is
`AwaitPrack`).

**Request record** (ADR-0055 §2, `call::RequestRecord`):
One request a dialog of the call holds open, by origin: **relayed** from the
peer leg (its received half is the originator's request, its sent half the
relay), this stack's **own**, **held** (received and owed a final a service
gives on a later turn; the platform gives a 500 at a deadline under the
sender's Timer F), or **received**, an INVITE this stack answers itself. It
carries its method, CSeq, branch, the request as sent and its destination, its
deadline, the dialog usage it belongs to and, for an own request, the rule
that sent it; a final settles it by its branch, outlives its dialog's
retirement, and settles by its final, its timeout, its leg's end, the call's
release or its deadline; a takeover seeds a transaction from each half. A
leg's dialog-creating INVITE has its record on the leg; a dialog holds the
INVITE it last sent and the one it answered 2xx awaiting its ACK.
_Avoid_: "pending relay" or "snapshot" for a record that is not a relay.

**INVITE exchange** (`call::InviteExchange`):
What an INVITE's request record carries past its final: the ACK its 2xx drew
(branch and datagram, RFC 3261 §13.2.2.4) or the ACK it awaits from the
acknowledging peer, the 2xx it answered awaiting its ACK (§13.3.1.4) and the
BYE queued behind that ACK (§15), the dialling facts of a leg's dialling
INVITE, and the final sent on a received one.

**Subscription record** (ADR-0055 §2, `call::Subscription`):
A subscription this stack is the notifier of on a dialog, a usage of its own
(RFC 5057 §3): its event package and `id` (RFC 6665 §4.5), the state its
NOTIFYs reported, and its NOTIFY awaiting a final. It opens with its first
NOTIFY and is **terminated** for good once a NOTIFY stating `terminated` is
asked for, or once a final that removes it or its NOTIFY's timeout ends it
(RFC 6665 §4.2.2): no NOTIFY leaves on it after, whoever asks. This stack's
NOTIFYs and BYE asked for while one of its NOTIFYs on the dialog awaits its
final are **queued** on the dialog, in order, and leave on that final or 8·T1
after the NOTIFY left.
_Avoid_: "own NOTIFY" for the record; "held" for a queued request (held is a
received request owed a final).

**Method table** (`b2bua_core::rules::method_table`):
What differs by method for a request record: what a dialog this side is
ending makes of it, the final a relay toward an ended leg owes, and whether
it waits behind this stack's own NOTIFY in flight or behind the ACK of this
stack's 2xx on the dialog (RFC 3261 §15). Knows no HA. What
the timeout of this stack's own request ends is policy: the default rule's,
which a service rule outranks for a request its rule sent (the record names
that rule).

**PRACK disposition**:
What a PRACK this stack receives draws on the face it showed the
provisional: relayed, answered 200 here, or refused 481. One reading serves
the live call and the released call's late-PRACK book.

**Dialog owed answer**:
The offer a dialog's responder made that this side still owes an answer to
(RFC 3264 §4), with the message that carries it: the ACK of a 2xx that
carried a delayed offer, the PRACK of the reliable provisional that carried
it, or that PRACK deferred to the ACK a 2xx awaits. One per dialog.

**Message ring**:
The capped per-leg history of the distinct SIP messages a leg received or sent,
in handling order, replicated with the call (`Leg.messages`, off by default,
`cdr.message_ring`). One entry per message: its direction (**received**;
**relayed**, a peer leg's message forwarded; or **authored**, minted by this
stack), method, CSeq, status, To-tag, the values of the configured header names
(`cdr.captured_headers`) and a call-wide `seq`. A rung, a re-ACK or a layer
replay records nothing: a repeat is the message it repeats; a UAS's periodic
re-sends of an unreliable 1xx are each recorded. The keepalive probe this
stack originates and its answer are not dialog history and are not recorded.
The transaction layer's own answers to a message the call does see (the 100
to an INVITE, a CANCEL's 200 and 487, the hop ACK of a non-2xx INVITE final)
are recorded as authored, as are the router's 481s to a stray request; a 100
the layer absorbs on a client transaction is not a message of the call's.
_Avoid_: "history" alone (the CDR vocabulary of a consumer), "log" (the
decision log is a different record).

**Decision log**:
The record of every decision the decision layer returned AND this stack
applied to a call, in application order, replicated with the call
(`Call.decision_log`). One **mark** per decision: its 1-based **ordinal**, the
turn's clock, its **kind** (the decision point crossed with the treatment:
route, reject, redirect, relay; failover route / reject / redirect /
terminate; release, release route; transfer allow / reject), the leg whose
event it answers, and the **label**: an opaque string the decision layer may
attach to any decision, recorded and read by nothing here. The count of marks
is the **decision ordinal**; every message-ring entry and every CDR event is
stamped with the ordinal standing when it was written, so the decision a
message was handled under is read from the log whatever a later decision
replaced on the call (a failover's `service_ext` is latest-wins); a late
message of an earlier leg is stamped with the decision current at handling.
`0` = before any decision (the caller's INVITE and its 100). A route the
limiter, the hop budget or the destination allow-list refused, an unanswered
consult (engine error, deadline), a limiter-refused reroute and every final
the stack authors on its own mark nothing. The async folds are marked once,
where the fold lands, before any rule reads it.
_Avoid_: "routing key" or any decision-layer meaning for the label (opaque
here); "decision count" for the ordinal (it is the stamp, not a statistic).

**Termination record**:
Who ended the call and why, replicated with the call (`Call.termination`),
written once by the first termination the call enters; a later termination
of a terminating call (the safety timer, a reaper verdict) leaves it. The
**cause** is a closed set: the peer's own BYE, CANCEL or final (**remote
bye**, **remote cancel**, **remote final**, which covers a callee failure let stand and a
481 denying the dialog); the decision layer's refusal of the call (**decision
reject**: a reject, a redirect, a relayed failure it authored, on the initial
path or on a failover) or its treatment's end of the call (**decision
release**: a release result, a route it answered that could not complete, a
media program run to its end); the duration cap (**max duration**); a
deadline by kind (**timeout**: a call-level completion deadline such as the setup
deadline or a transfer's overall guard; no answer; PRACK; ACK; keepalive, the
probe unanswered by its own Timer F or answered with a dialog-terminating
final; transaction); the stack's own
refusal (**admission**: a limiter, the destination
allow-list, a spent hop budget, a malformed INVITE, an unreadable or unanswered
decision); the per-call message cap; the **supervisor** (a reaper strike, a
forced terminal). The record names the leg whose message or timer caused it
(**by leg**: the caller for its BYE or CANCEL, the callee for its BYE, final
or unanswered probe; none for a decision, the stack or its supervisor), the
turn's clock, and the **cut** (`last_seq`): the `seq` of the last message-ring
entry the terminating turn recorded, so every entry at or under it was
received or sent as part of beginning the termination (the peer's BYE and its
200, the BYE or CANCEL relayed to the other leg, the 487, the caller's final
where a deadline ends an unanswered call) and every entry above it came after
(the other leg's 200 to that BYE, the ACK to the 487); `0` while the ring is
off. Every action that begins a termination states its cause
(`BeginTermination`, `TerminateCall`), as does every direct path to terminal
(a decision reject, an admission refusal, the reaper's discharge); a terminal
reached without one is counted (`termination_unrecorded`).
_Avoid_: "reason" for the cause (the free-text `reason` is a label the rules
pass, an RFC 3326 value at most; the cause is typed); "snapshot" for the cut
(the ring is whole; the cut is a `seq`).

**Snooze** (`RuleAction::SnoozeTimer`):
A timer fire that a rule puts off by one short step instead of acting on, so the
same timer fires again. The call's timer ledger counts the snoozes of a key, and
arming the key fresh resets the count, so the rule can bound how long it puts off
a deadline.
_Avoid_: "defer" (the ACK an offer is deferred to, the copy that defers to its
primary); "hold" (call hold, limiter holds); "retry" (a request sent again).

**Admission ladder** (ADR-0037, `b2bua::admission`):
The ordered **rungs** that may refuse a new INVITE: the ingress brake, the
transaction layer's backlog, capacity, the shed at the per-call queue cap,
the panic-ELU backstop, the CPS bucket. Each rung keeps its site and is
decided by one function over its input and the INVITE's **class** (normal,
emergency, or in-dialog for a To-tagged INVITE, judged by the backlog
alone). A refusal draws the one 503 and is counted once on
`b2bua_new_calls_total`; a later copy of a refused INVITE is a **refused
copy**, counted apart.
_Avoid_: "tier" (the rungs are one ordered table); "rung" alone where a
retransmission ladder's rung is meant (context decides).

**Copy** (of a new INVITE, RFC 3261 §8.2.2.2):
An initial INVITE matching an admitted or live call's Call-ID, From-tag and
CSeq on another branch: not judged, never a new call, answered 482. The same
Call-ID and From-tag with another CSeq is a **new request** (a retry after a
challenge, §8.1.3.5), judged as any new INVITE.
_Avoid_: "retransmission" for a copy (a retransmission matches its server
transaction and never reaches the router).

**Capacity gate** (memory admission, ADR-0037):
The worker's bound on what it holds: live calls, live SIP transactions and
process RSS, each with a **normal ceiling** (a new non-emergency call is
refused at it) and an **emergency ceiling** (every new call is refused at it).
A refused call draws a 503 with a jittered `Retry-After` and no `Reason`, and
no call state is born. The **backup ceilings** (a count and an RSS value) keep a
replica of a call not yet held out of the store; a replica left out this way
is **shed**: the backup refuses it in a frame to the primary, its position
moves past it, and the call is **unbacked** for life (ADR-0055 §3).
_Avoid_: "overload" for these rejects (overload is the rate and loop shedding
of the CPS bucket, the panic-ELU backstop and the queue brake); "limit" for a
ceiling (the call limiter owns that word).

**CPS bucket** (the admission ladder's last rung):
The worker's token bucket on new-dialog INVITEs, refilled at `cps_bucket_rate`
up to `cps_bucket_size`; an empty bucket refuses a non-emergency call with a
503 whose `Retry-After` is jittered over the larger of the configured base and
the time to the next token. A token is spent only
when an admitted INVITE's turn is queued; a refusal spends none. An emergency call spends
a token when one is there and passes an empty bucket owing nothing, so the
level stays in `[0, size]` and, with size and rate at least 1, non-emergency
admission resumes `1 / rate` seconds after any surge.
_Avoid_: "overdraft" or "debt" (the bucket has none).

**Load-shedding primitives** (`load-shed`):
The parts every shedding process shares: one clock-free token bucket (the
caller passes `now`), the EWMA over a load reading, the `LoadSampler` read seam
and its simulated control, and the `Retry-After` policy (a 1 s floor and a
uniform jitter over a caller-supplied roll). Each process keeps its own refusal
order; the worker's admission ladder and the front proxy's self-gate and per-worker caps
are built from these. A refusal made before any transaction derives its To-tag
and jitter roll from the request (`sip_txn::StatelessTagger`), so every copy of
one request draws the same answer.
_Avoid_: "rate limiter" for the bucket (the call limiter owns "limit").

**Proxy refusal** (`sip_proxy::refusals`):
A request the front proxy refuses itself, at its self-gate or for a failed
worker selection, draws a 503 with a jittered, floored `Retry-After` and no
`Reason`, its To-tag derived from the request. It is remembered for 64·T1:
every copy in that window draws the same bytes and is neither judged nor
counted again, so a gate or a cap that cleared meanwhile never admits it. The
CANCEL of a refused INVITE draws a 200 from the proxy, that INVITE's UAS, and
goes no further; a CANCEL is never remembered. Why it was refused stays in the
reject counters and the call's trace.

**First worker view** (`sip_proxy::health::FirstView`):
A starting front proxy's view of its workers is in once the worker registry
holds its first listing and one OPTIONS round over it has completed, each
listed worker having answered or run out its reply window (ADR-0050 §11).
The round leaves as soon as the listing is in. Before the view, a request
with no target is dropped and counted, never refused: its retransmission meets
the view. `/readyz` waits for the view, and the proxy's node announces its
addresses only while `/readyz` passes (ADR-0056); `/healthz` never waits for it.
_Avoid_: "warm-up" (there is no fixed delay: the view is in when the first
round has answered or run out its reply window).

**Metric catalogue** (`metric-catalogue`):
A process's Prometheus families declared as data: one **catalogue entry** per
family (name, kind, label sets, presence, help), its label sets a union of
products of label dimensions, a dimension's values taken from an enum's
exposition order. A renderer writes a family from its entry and one value per
series; a test checks a scraped text against the entry. Every declared label
set is written from the first scrape, at 0 before its first event. A
**fixed** family has no other; a **semi-open** one also writes a label set
nobody declared (an extension method, a peer) from its first observation,
under its own labels, never folded into another value. Where those values are
unbounded the family has a **cap**: past it an observation lands on an
**overflow series** (its capped labels `_overflow`, the others kept) and is
counted on the cap's overflow counter. One **catalogue** per shipped binary lists its `/metrics`
families in order; `metric-contract` exports them as JSON and checks the
names and selector labels a reader spells against them.
_Avoid_: "registry" (nothing registers at run time; the process owns its values);
"other" for an undeclared value (it keeps its own label).

**Dispatch class**:
What one event is to its call's dispatch queue, derived once from the event: a
new normal INVITE, a new emergency INVITE, an in-dialog INVITE, an ACK, another
request, a stray CANCEL, a response, a client transaction's outcome, a
`Cancelled` notice, a timer fire, an internal event. The class's row of the
dispatch table states everything the queue decides: the event's **room**
(bounded, refused at a full queue or the global cap; past bounds, waiting up to
the call's overflow ceiling; or always, lost only behind the call's release
with no new call waiting),
whether it counts toward the call's lifetime cap and keeps its room past it,
its permit pool (a normal new call's turn also holds one of the new-call
share), the live-queue count below which it opens its call's queue (a normal
new INVITE stops at the new-call headroom below the cap), and the **owed
answer** of each discard. A new kind of event is one class and one row
(`dispatch/class.rs`).
_Avoid_: "must-run" (a room, not a class), "job flags".

**Owed answer**:
What the router owes the sender of a request its call's queue discarded
unrun, by the discard's reason: an answer through the request's transaction
(the capacity 503 to a new INVITE, 500 or 481 to an in-dialog one, a capped
call's refusal), a forget of the transaction so its retransmission is
admitted afresh, or nothing. Exactly one per discard; the queue hands the
event back with it and the router renders it.
_Avoid_: "discard hook" (the answer is a value, not a callback); the
**Dialog owed answer** is RFC 3264's, not this.
