# PCAP2TEST_PIVOT_V3.md — pivot scenario format, v3 (NORMATIVE)

`pivot_version: 3`. There is no compatibility with an earlier version: the corpus is
pcap-generated and regenerates.

The pivot is the declarative artifact between a capture and a replayable test. It is one
format for two things: a replayed capture, and a test a human wrote. A captured call is the
degenerate straight-line case, an authored test uses the extension constructs, and one
interpreter runs both. The generator emits a strict subset, and lint refuses a captured
document that carries anything outside it (§13.1).

Design rule: **smart compiler, dumb interpreter.** The generator compiles a corner case into
explicit fields; the interpreter never infers one at replay time. Callflow knowledge inside an
interpreter is a design failure of the format.

**What this file holds.** The format's invariants, the contracts between the components that
read and write it (generator, lint, driver, lane, interpreter, confrontation), and what the
format deliberately does not carry. It does not restate fields: `crates/pivot-schema` owns the
contract, its serde structs and their JSON Schema export (`pivot-schema schema pivot`) are the
source of truth, and where this file and the structs disagree the structs govern. Each section
names the module whose doc comments carry the detail. The TS mirrors (`@sip/contracts`) are
checked against the committed `crates/pivot-schema/tests/fixtures`. A change to the format lands
in one batch across the structs, this file and the generator.

## 0. Out of scope

### 0.1 What the format does not carry

- **REFER with `Replaces`** (RFC 3891, attended transfer). No construct states it, and
  `actors[].claim.by` has no discriminator for an INVITE that names another dialog. The
  generator excludes such a call group loudly (`ts/pipeline/src/refer-replaces.ts`).
- **A per-call setup deadline.** The wire shows only the cancel timing, which
  `attempts[].no_answer_ms` states (§4.1).
- **Control flow.** No loops, conditionals or jumps; an `alt`'s declared alternatives are the
  only branching. Authoring sugar (matrices over topology axes, repetition) lives in the
  TypeScript that generates documents and is always unrolled.
- **The scene.** Hundreds of scenarios share one scene and the runner pairs scenario × scene;
  the document states only what it needs (`case.requires`, §3.2).

## 1. Case directory layout

```
<case-id>/
  scenario.json          # this document
  resources/             # bodies and multipart parts, one file each (§8.4)
```

The fixtures in `crates/pivot-schema/tests/fixtures/` are the worked examples; the
`authored-*.v3.json` ones use the authored half of the format.

## 2. Top level

Fields: `crates/pivot-schema/src/document.rs`. `deny_unknown_fields` on every object, so a
misspelling fails loudly. `postconditions` is optional in the schema and required by lint
(§10).

### 2.1 Canonical serialization

Keys sorted lexically at every level, two-space indent, one trailing newline. The formatter
(`pivot-schema fmt`) owns it and parses through the structs, so formatting a non-conforming
document fails. Generator output and hand edits are formatted before any diff
(`crates/pivot-schema/src/canonical.rs`).

### 2.2 Emptiness

Every optional collection is OMITTED when empty: never `[]`, `{}` or `null`. No field's
emptiness carries meaning.

## 3. case

Fields: `crates/pivot-schema/src/case.rs`. Lane names, blocking reasons, capability tokens and
the `family` label are deployment vocabulary, modelled as open tokens; only the
`ok` / `blocked:<reason>` grammar is fixed. The generator states each lane's verdict, lint
checks the claim, and the driver skips a blocked lane loudly instead of failing it.

### 3.1 origin

`origin` is what the generator-subset gate turns on (§13.1). A captured document is a
projection of packets a tool saw: everything it carries is readable off them, and it carries
what they justify (`source`, per-step `observed`, `timing.capture_span_ms`). An authored
document may use the whole format and carries no capture coordinate.

### 3.2 requires

Informative open capability tokens; the interpreter never reads them. They make an impossible
scenario × scene pairing refuse loudly. A token may imply scene parameters the document does
not carry (`store-faults` a fault profile, `ha-pair` a node count): the document states the
need, the scene states how it is met.

## 4. calls

Fields: `crates/pivot-schema/src/call.rs`. Each call owns the ONE encoding of its attempt
chain: parallel forks are same-`position` attempts on different `branch`es, a sequential hunt is
one branch with several positions. A captured case is exactly one call (a capture yields one
document per call); `calls[]` plurality is for what really is more than one call, a
call-limiter test or a consultation placed beside the call it will transfer.

### 4.0 refused / abandoned — the call that dialed nobody

An empty chain states one of two facts, never neither. `refused`: the routing decision answered
the caller a failure final without dialing. `abandoned`: the caller cancelled before any called
leg crossed the vantage, and the `487` that follows is what RFC 3261 §9.2 owes, not a decision.
Each names the caller-leg step that carries the final or the CANCEL; the step holds the status
and headers, so the two cannot disagree. A refused call is its whole lowered program: no ring,
no provisional profile, no egress (§4.3). Lint codes: `call/*`, `ref/*`.

### 4.1 attempts

**A call is one calling leg plus n called legs.** Every `cause` but `closed:bye` is a FAILURE,
so a chain states a sequential hunt. `closed:bye` is an attempt that ANSWERED and that the
platform later released; an attempt after it reroutes a released leg. A transferee leg or a
media resource inserted mid-call is no failure: it JOINS the call on a branch of its own and
states `joined_by`, whose `step` is the flow step that performed the join. That step is on a
leg of the same call, runs unconditionally (not inside an `alt` branch) and precedes the joined
leg's first message.

**`joined_by` and `cause` are orthogonal.** A joined leg fails like any other: a reroute after
a join, or an automatic transfer to a busy target that the platform reroutes internally (one
attempt with `joined_by` and `cause: busy`, then the next position of the same branch).

**A `cause` cites only the attempt's own dialog-creating final, or an actual closer (a BYE or
a CANCEL).** An in-dialog final answers a renegotiation and closes nothing, so it is
`cause_evidence`, never a cause; what made the platform send a BYE is evidence too. Lint checks
the rule through `in_dialog` (§6.1): `cause/in-dialog-final`, `cause/closer-missing`.

`no_answer_ms` is declared only where the attempt RANG and nothing else explains the give-up:
from the first 180–189 the attempt's leg drew to the SUT's CANCEL, the instant a lane arms its
ring deadline from. A caller that answered or hung up mid-attempt makes that CANCEL a
propagation, and the cause is refused. The armable band is `NO_ANSWER_MS_BAND`.

### 4.2 relay18x

The provisional-handling profile the CAPTURED system ran for this call, in the platform's own
vocabulary (open tokens, checked against no list) so a lane applies it without re-deciding.

### 4.3 Per-lane compilation contract

**The driver compiles `calls`. The interpreter never interprets it.** Before a run, the TS
driver lowers each call into whatever the target lane needs:

- every lane: a binding from each `identities` name to a real number. The translation is the
  lane's (a mock's allocation and a provisioned backend's leased number differ for one entry),
  and it is what `${num:…}` resolves through;
- mock lanes: directive headers injected on the outbound INVITE (attempt order, causes, dwells,
  provisional profile);
- a real backend: provisioning choices, leased numbers, route configuration;
- the fake lane: number allocation per attempt and the armed no-answer timers.

The interpreter receives the compiled run configuration (`crates/pivot-schema/src/bundle/runconfig.rs`)
and the flow, and never reads `cause`, `cause_evidence`, `join_evidence` or `relay18x`.

- A lane numbers by attempt INDEX, so every branch's attempt `s` dials one number; two claims
  sharing an endpoint stay ambiguous (`claim/same-number-ambiguous`). A per-attempt timer is
  always strictly below the global bound.
- A lowered route plan states branch 0's chain and nothing else: a joined branch is dialled by
  whatever joined it.
- A lowered directive is PER CALL. What the lane states about egress (destination, route plan,
  admission entry) is keyed by `calls[].id` and reaches only that call's dial, the INVITE that
  opens its caller leg. What it states about the run (a test-correlation header, a lane
  artifact) stays run-level.

### 4.5 Tier-2 position tokens

`caller` and `called[<branch>][<position>]`, qualified with the call id once a document
declares more than one call (`crates/pivot-schema/src/call.rs`, `Position`).

## 5. endpoints, actors, legs

Fields: `crates/pivot-schema/src/placement.rs`. An endpoint is one mux socket, an actor one
simulated element on it, a leg one symbolic dialog. Call-ID, tags, CSeq base and route set are
runner state, reached only through the leg id and the leg accessors (§8.1). How a UAS claims
its inbound INVITE: `crates/pivot-interpreter/src/claim.rs`.

### 5.1 background

Runtime contract: `crates/pivot-interpreter/src/background.rs`; what a capture collapses into a
policy: `ts/pipeline/src/background.ts`.

- The full shape is AUTHORED ONLY, with one generated exception: exactly
  `{ "match": { "method": "OPTIONS" }, "respond": { "status": 200 } }`, no `count`, on every
  actor the deployment's policy says answers the replaying SUT's in-dialog OPTIONS audit. The
  captured platform's own locally-minted exchanges of that method leave the flow (§6), flagged
  `background-claimed`.
- A policy yields to the flow: an arrival a frontier `expect` on that leg is open on (its budget
  started, §6.8, its discriminator satisfied) is that step's.
- `count` is checked at settle, over the whole run, settled or not. A count of ABSENCE is
  evidence only where the lane's egress could reach the party it counts, which is why the lane
  states each call's own egress (§4.3).
- `match` is method-only. A policy that inspected a header would be a flow step in disguise.

## 6. flow

Node kinds and nesting: `crates/pivot-schema/src/flow.rs`; run-time ordering:
`crates/pivot-interpreter/src/cursor.rs`, `program.rs`. Block ids and step ids share one
namespace.

**A captured flow lists EVERY captured message**, protocol automatics included, so a reader can
tell "elided automatic" from "never happened". The one exception is an exchange a `background`
policy claims (§5.1). A captured retransmission is no step: it collapses onto the step it
repeats as a count (§6.9).

### 6.1 A message step

Fields: `crates/pivot-schema/src/flow.rs`, `Step`. There is no `expect-claim` op: a UAS's first
inbound INVITE is a claim by virtue of its actor's `claim`.

**`in_dialog` is TOTAL and positional.** A leg's dialog-creating final is the first `2xx` to an
INVITE on that leg. Every step strictly AFTER it carries `in_dialog: true`, every step at or
before it carries none; a leg whose INVITE never took a 2xx carries none anywhere. Strictly
after, because the dialog exists once that 2xx's To-tag arrives, so its ACK is already a request
within the dialog (RFC 3261 §13.2.2.4). The method never substitutes for the marker. Two
exclusions: a CANCEL and every response to one (§9.1, §12.2), and an `alt` branch step reads
only the finals its own run reaches. The marker feeds §4.1's citation rule; lint enforces it
(`in-dialog/*`, `crates/pivot-schema/src/lint/flow_rules.rs`) and the interpreter gates nothing
on it.

**`confirms_dialog` marks the ACK that answers a dialog-creating final, once per DIALOG.** It
rides beside `in_dialog` (and beside `early` where the ACK names its fork); under forking a leg
answered under two To-tags carries it twice. An ACK to a re-INVITE's 2xx and an ACK to a non-2xx
final never carry it. Which ACK answers the final is read by leg state, by position, never by
CSeq (`crates/pivot-schema/src/lint/transactions.rs`). The marker is required: a confirmed
dialog whose answering ACK states none is an error.

**`in_dialog` and `early` are orthogonal.** `in_dialog` means CONFIRMED, so every early-dialog
message (a reliable provisional, its PRACK, an UPDATE before the final) takes no marker, and
`early` alone says which fork it rides. They co-occur on a further dialog of the leg: a later
fork's own 2xx and the ACK for it.

**An `early` id names one fork.** A response `send` answers under it, minting the fork's To-tag;
an `expect` gates on it, so only the named fork's tag satisfies the step. A request a leg sends
before any 2xx rides the fork `early` names, else the leg's one ringing early dialog, else its
one early dialog a non-2xx final ended (the far side answers 481, §12.2.2). Several ringing forks
and no name is a refusal. Readings and refusals: `crates/pivot-interpreter/src/early.rs`,
`stack/request.rs`.

### 6.2 Step identity

Every reference in the document is id-based (`delay.from`, `after`, `deviations[].step`,
`case.defect.marker.step`, `${step:…}`), never positional, so inserting a step moves no marker.
A capture's ids are `s1`, `s2`, … dense and 1-based. An id carries no `.`: the accessor grammar
splits an id from its field at the first dot.

### 6.3 auto steps

`auto: true` marks a message the interpreter's stack COMPOSES: 100 Trying, the ACK to a final,
PRACK and its 2xx. The stack derives its R-URI, Route, Via and CSeq from the TRANSACTION that
obliged it, not from dialog state (`crates/pivot-interpreter/src/stack/request.rs`,
`stack/respond.rs`).

- **Storage is unchanged.** An auto step stores what any step stores (§8). The one thing
  withheld is a body the stack has nowhere to place: a 100 Trying and an ACK to a non-2xx final
  carry none (`auto/body-not-composable`); the ACK to a 2xx (where a delayed offer's answer rides)
  and PRACK with its 2xx do.
- **`cseq` is a pairing token.** It is the CSeq number the capture carried, used for
  confrontation, for lint and to scope an auto ACK's drawn `retransmits` to one transaction. It is
  never replayed and never resolved against: an auto ACK finds its final in leg state. It never
  appears on a scripted step.
- An auto `expect` always carries `check: "record"`; an auto `send` carries no `check`.
- A held automatic is the step's measured `delay`. A WITHHELD one is the step plus a
  `suppress-auto` deviation (§11).
- **An auto ACK's `retransmits` is DRAWN, not paced**: one ACK per repeat of the final its own
  transaction drew. On an `expect` the generator derives the count from the final's own ladder,
  never from the ACKs the capture held, and flags it `ack-count-drawn-from-final`; a `send` keeps
  the captured count, which models the peer. The rules (relayed ACK to a 2xx, displaced by the
  next INVITE, composed on arrival for a non-2xx): `ts/pipeline/src/drawn-ack.ts`,
  `crates/pivot-interpreter/src/retransmit.rs`.
- A leg that took a non-2xx INVITE final and whose flow states no ACK for it sends that ACK
  itself (`crates/pivot-interpreter/src/exec/unscripted.rs`).
- An auto PRACK acknowledges the reliable provisionals outstanding on its leg oldest first.

### 6.4 check: assert or record

`crates/pivot-schema/src/flow.rs`, `CheckMode`. The generator asserts a message some captured
peer emitted and the SUT relayed, and records a message the SUT MINTED. A lane never
reinterprets a stored field. Scoping a header only the origin platform emits is per HEADER, a
class (§9.1), never a weaker `check` on the whole message. Under both modes the body TYPE is held
(§8.3).

### 6.5 Acceptable nondeterminism — AUTHORED ONLY

`alt` (which of several things happened), `optional` (tolerated absence) and `unordered` (all
arrive, order free). Run-time meaning: `crates/pivot-interpreter/src/cursor.rs`; structural rules
(discriminability, no `send` or `optional` opening a branch, an `unordered` holds two or more
expects): `crates/pivot-schema/src/lint/flow_rules.rs`.

The release rules are normative. An `optional` expect is released when a later step on its leg
MATCHES first with no pending required expect or unsent send in front of it, and released, never
failed, when its own budget expires. A send releases nothing. A response `send` anchored on a
released `optional` request expect of its own leg is moot: nothing goes on the wire and the step
completes.

Branch scoping: a step inside an `alt` branch exists only on the run that chose that branch, so
from outside the branch a reference names the alt node's own id ("the alt completed, whichever
branch ran"); `order/cross-branch`, `accessor/cross-branch`. Every `unordered` member arrives on
every run, so a later step may name any of them; two members may not name each other.

### 6.6 inject — AUTHORED ONLY

An external event (store fault, HTTP-fabric fault, node kill) handed to the lane's injector;
`action` is an open token from a deployment-owned registry. The interpreter never executes an
action, and setup-time knobs belong to the scene (§3.2). Contract and failures
(`inject-refused`, `inject-timed-out`, `injector-missing`):
`crates/pivot-interpreter/src/injector.rs`, `exec/injects.rs`.

### 6.7 after — AUTHORED ONLY

The one cross-leg and cross-call ordering device; same-leg order is list order. Ordering is
message-mediated only: "after leg B confirmed" is a reference to the ACK step. A reference points
backwards and obeys §6.5's branch scoping. A captured document carries none: its chain barrier
follows from `attempts[].leg` plus `position`.

### 6.7a overlap — the declared race

Two NEIGHBOURS on one leg arm together, and whichever the wire settles first settles first. The
generator declares one where the captured order is the difference between a dwell the document
holds and a latency it does not (`ts/pipeline/src/race.ts`); the interpreter refuses a non-adjacent
or cross-leg pair (`crates/pivot-interpreter/src/plan.rs`).

### 6.7b The undeclared race: a relay behind a send

A relay (an `expect` at a synthetic zero from a completed step on another leg) arms beside a
`send` its leg has not made yet (`crates/pivot-interpreter/src/cursor.rs`).

### 6.7c The answer to a transaction already open

An `expect` naming a response to a transaction its leg already opened arms beside what stands in
front of it; RFC 3261 §17 orders no two answers on two open transactions. Bounds:
`crates/pivot-interpreter/src/cursor/transaction.rs`.

### 6.7d The answer to a CANCEL sent after its INVITE's non-2xx final

200 and 481 each satisfy an `expect` naming the other (`crates/pivot-interpreter/src/cancel.rs`).

### 6.7e The answers a received CANCEL draws

The `200` to a received CANCEL and the INVITE's non-2xx final go out past whatever stands in
front of them (`crates/pivot-interpreter/src/cursor/transaction.rs`; an unscripted CANCEL and the
reject ACK: `scripted_final.rs`, `exec/unscripted.rs`).

### 6.7f A request naming no dialog the leg holds

Answered 481 and failed as unexpected (RFC 3261 §12.2.2), mid-flow and during settle alike
(`crates/pivot-interpreter/src/exec/unscripted.rs`).

### 6.7g awaits — a lane event a send waits on — AUTHORED ONLY

Some orderings a test needs are against something the SIP wire does not show, such as a CANCEL
sent while the system holds a request to its decision backend. `awaits` names a lane event; one
reported occurrence releases one send (`crates/pivot-interpreter/src/events.rs`,
`exec/awaits.rs`). What produces the event and holds the request in flight is the lane's. The one
token this file names:

| token | the lane reports it when |
|---|---|
| `decision-request-in-flight` | the system under test sent a request to its decision backend and holds no response to it yet |

### 6.7h What a final or a request the leg took is owed

An ACK `send` whose INVITE final an `expect` of its leg has taken goes out past an arrival the leg
still awaits, and so does a response `send` to a non-INVITE request an `expect` of its leg has
taken; a send in front keeps its place. Held that way, the ACK to a non-2xx final goes out when
the final arrived (RFC 3261 §17.1.1.3), the ACK to a 2xx at the document's offset from the 2xx
(§13.2.2.4), and the answer at the document's offset from its request (§17.2.2). An INVITE's
answer keeps list order: RFC 3262 §3 orders a final behind the PRACK it waits on. The expect a
held send passed still fails on its own budget
(`crates/pivot-interpreter/src/cursor/transaction.rs`, `exec/flow.rs`).

### 6.8 delay and dwell

`crates/pivot-schema/src/flow.rs`, `Delay`. Every message step carries a `delay`, auto steps
included. All delays are relative to an explicit anchor that points backwards; there are no
absolute times. `compressible` and `timer_linked` are STATED, never derived; a timer-linked dwell
is never compressible. How the generator anchors and classifies a delay:
`ts/pipeline/src/delay.ts`.

**The dwell is what the two ops share.** On a `send` it is slept. On an `expect` it fixes when
the step's budget OPENS (`anchor + ms`, or when the leg reaches the step, whichever is later);
before that the step is matchable but cannot time out. It never widens the match and never moves
the §9.2 comparison. An anchor may name a block, which settles when it completes.

### 6.9 retransmits

A captured retransmission collapses onto the step it repeats as a count. Run-time contract
(pacing, drawing, counting, the four assertion regimes, the RFC as the oracle on every lane):
`crates/pivot-interpreter/src/retransmit.rs`.

- **A repeat is the SAME DATAGRAM, byte for byte, inside its transaction's 64·T1 envelope.**
  Matching bytes past the envelope are a re-emission with a step of its own. The extractor's
  ingest dedup window removes capture-stack duplicates and nothing else.
- **An unreliable provisional** (1xx above 100, no `RSeq`) rides no timer and states no repeat:
  a platform that rings twice has sent twice.
- On an `expect`, `retransmits` is recorded evidence of what the capture held; the ladder the run
  owes is the RFC's, settled by the interpreter.
- The system's ACK to a 2xx the run sent owes one repeat per copy of that 2xx sent after its first
  ACK arrived (RFC 3261 §13.2.2.4), none for a copy before it; a copy that crossed the ACK in
  flight may draw one or none. A re-ACK beyond those, up to the count the document declares, is
  the capture's own: a relay repeats the re-ACK its other leg took.
- `retransmit_intervals_ms` states a captured ladder's own pacing; absent, the class's RFC pacing
  applies.
- A peer that re-ACKs each repeated final on a fresh branch mints distinct datagrams: each ACK is
  its own step.

What the generator derives so a capture replays against a relaying SUT, each named by a flag
(§13.2):

| derivation | module |
|---|---|
| a caller-facing provisional the SUT is not caused to emit becomes `optional` (the one `optional` the subset admits) | `ts/pipeline/src/spare-provisional.ts` |
| a relayed provisional the capture lacks is derived from the emission that causes it | `ts/pipeline/src/mirrored-provisional.ts` |
| a relayed arrival is listed after the emission it relays | `ts/pipeline/src/relay-placement.ts` |
| the far side of an in-dialog INVITE exchange the vantage lost is transcribed onto the far leg | `ts/pipeline/src/far-side-reinvite.ts` |

A platform running a non-transparency mode (one 18x by design, a rewrite, a managed PRACK) is
outside all of these. A count alone never states such a mode. Every rule here reads the message
CLASS, never the status: a 180 and a 183 without `RSeq` take one path.

### 6.10 observed

`crates/pivot-schema/src/flow.rs`, `Observed`. Required on a captured document, absent on an
authored one, never read by the interpreter. The confrontation pairs a step with its captured
message by LOOKUP, so two steps may name one captured message where the SUT emits it twice (a
derived provisional, a far-side re-INVITE).

## 7. timing

`crates/pivot-schema/src/document.rs`, `Timing`. A generated document states both budgets as
64·T1 plus a 5 s decision margin, so the runner always outlives the SIP timer it watches
(`ts/pipeline/src/assemble.ts`). `capture_span_ms` is the case's span for a reader to compare a
run against, not a run ceiling. What bounds a run is wall time: the stall detector gives one run
120 s (`crates/pivot-interpreter/src/exec/stall.rs`).

## 8. msg, tiers and accessors

Fields: `crates/pivot-schema/src/msg.rs`; tier data: `tiers.rs` (§8.2).

- **Tier 1, stack-owned**, never stored: Via and branch, Call-ID, From/To tags, CSeq numbering,
  Max-Forwards (unless stated), Content-Length, Contact host:port, route sets. A request whose
  `headers` state a `Max-Forwards` sends that hop count once.
- **Tier 2, role-mapped**, stored symbolically: positional refs (`{ pos, form }`, §4.5) or
  `{ frozen }` where the plan does not recognise the value. The party a positional ref names is
  declared once, in the identity registry (§8.5).
- **Tier 3, frozen**, stored verbatim in wire order.

**The dialog a scripted request rides is tier-1 state** (`crates/pivot-interpreter/src/stack.rs`).
Only a 2xx to a dialog-creating request opens a dialog; a provisional under a To-tag opens an
early one, which a non-2xx final ends (RFC 3261 §12.1, §12.3). The remote target moves only on
the Contact of a dialog-creating or target-refresh request, of a 2xx to one, or of a provisional
creating an early dialog (§12.2.1.2). A request with no dialog to ride, or riding one with no
remote target, fails its step by name.

Two splits by direction. `RSeq`: a send freezes the captured value, an expect states only
presence (`headers-present`), since the value is stack-owned per leg (RFC 3262 §3). Body
descriptors (Content-Disposition, Content-Encoding, Content-Language, MIME-Version): a send
freezes them, an expect whose captured message has no body states none of them.

### 8.1 Accessors — AUTHORED ONLY

Grammar and namespaces (`leg`, `early`, `step`, `num`):
`crates/pivot-schema/src/accessor.rs`; resolution: `crates/pivot-interpreter/src/resolve.rs`;
what lint requires of each: `crates/pivot-schema/src/lint/accessors.rs`. The contracts:

- accessors resolve against runner state, never document text, so a compiled document stays
  immutable and shared across call instances;
- an accessor is looked for in EVERY string a document carries (inventory:
  `crates/pivot-schema/src/lint/strings.rs`); no position treats `${…}` as literal text;
- arithmetic is `{ from, delta }` and nothing else;
- a body is only ever a match target, never extracted and resent;
- `${num:<name>:<form>}` names a registered identity in a form it declares, so a number-bearing
  header (`Refer-To`, `P-Asserted-Identity`, `Diversion`, `History-Info`, `Contact` …) composes
  per lane instead of freezing one capture's number.

### 8.2 Tier data is NORMATIVE DATA, not prose

`pivot-schema tiers` exports the tier-1 omission list, per-message From/To/Contact handling, the
compact-form header map and the generic body-handling registry; generator and interpreter read
the same export (`crates/pivot-schema/src/tiers.rs`). A deployment that rewrites numbers inside
its own body format registers that type in its own overlay.

### 8.3 Bodies and multipart

Shapes: `crates/pivot-schema/src/body.rs`. The contracts between components:

- **Extraction splits multipart.** The flows emitter writes each part out; no pivot consumer owns
  MIME.
- **A body is emitted as the document holds it.** Every part's payload, `content-id` and entity
  headers ride byte-exact and in order. Two contents are rewritten: SDP (`c=addr`, `m=port`, from
  the lane's media booking, `crates/pivot-interpreter/src/media.rs`) and an MSCML `<response>`'s
  `id`, which echoes the oldest unanswered request of its type
  (`crates/pivot-interpreter/src/mscml.rs`). The one regenerated item is the multipart
  `boundary`. A `content-type` is stored verbatim with its parameters; the only value left
  unstated is bare `application/sdp` on a rewritten body.
- **The rewrite tokens name what the lane MAY rewrite.** A lane without media runs a verbatim
  booking, and its run configuration says so (`"media": "verbatim"`; absent reads `rebooked`).
- **The interpreter holds the body TYPE on every expect**, under both `check` modes: presence,
  bare media type and, for a decomposed multipart, part count and part types by position
  (`crates/pivot-interpreter/src/gate/body.rs`). Under `assert`, presence and the container type
  gate the match; any other miss is a `check-failed` finding that keeps the datagram.
- **The CONTENT is the post-run confrontation's**, never the interpreter's, so a differing body
  never abandons the call. `compare` (`exact` when absent, `xml`, `sdp`) says how a received body
  is held against the resource; it rides an expect and no send
  (`ts/pipeline/src/bodyfold.ts`, `sdpfold.ts`; records: `confront.ts`).
- Per-part handling is a registry on the bare media type: `application/sdp` is rewritten, a known
  non-SDP type is frozen, an unrecognised one is frozen and flagged when its payload carries
  number-like digits (`ts/pipeline/src/bodies.ts`).

### 8.4 Resource naming

`resources/<actor>_<n>_<part>.<ext>` on a send, `resources/<actor>_r<n>_<part>.<ext>` on an
expect, step-index-free (`ts/pipeline/src/flowsteps.ts`, `slug`).

### 8.5 The identity registry

`crates/pivot-schema/src/identity.rs`. Every number and domain the document names is declared
once; the registry names and classifies, the driver binds per lane (§4.3). Three sites name an
entry: `attempts[].callee.identity`, `actors[].identity` and `${num:…}`.

- A generated document names parties by tier-2 position: `caller`, `called-<branch>-<position>`.
  A document declaring several calls qualifies them with the call id (`c2-called-0-0`,
  `id/identity-unqualified`). A party the chain never dials takes any name.
- `observed` is capture provenance and is anonymised on a captured document. An authored test may
  state numbers freely; the lane binds the name, never this field.
- A capture may carry the registry and `${num:…}` compositions, which resolve statically through
  the names and forms the document itself declares (§13.1).

## 9. checks

`crates/pivot-schema/src/check.rs` (shape), `crates/pivot-interpreter/src/checks.rs` (the field
selectors and their readings). One vocabulary, inline on an `expect` and in `postconditions`, both
authored-only. A `${…}` inside a `regex` value is substituted as a literal.

### 9.1 Check classes and lane scoping

**The document carries facts; what a fact costs on a lane is decided outside it.** A class names
which assertions read the origin platform's vocabulary rather than SIP's, and it is stated, never
inferred (`crates/pivot-schema/src/scoping.rs`). Two members, closed: `origin-platform-header`,
`cdr-vocabulary`. A class rides a `checks` entry or a frozen header in `msg.headers`;
`headers-present` carries none. The generator classifies from the deployment's header families
and from an asserted value no capture-side `send` of the document carries.

The rule a run applies, and the only one: **when the run's lane is not `case.origin_lane`, a
classified check is evaluated, recorded and does not gate.** A run configuration may override a
class outright in either direction (`check_scoping`). Anything more conditional is a driver pass
over the document before the run. A downgrade is no skip: a failing informative check lands in the
verdict's `informative` section, and there is no third status
(`crates/pivot-interpreter/src/scope.rs`).

### 9.2 Timing tolerance — the run's window, not the document's

A `timer_linked` dwell states what the capture measured; the RUN states the window it accepts
(`timing_tolerance_ms`, `crates/pivot-schema/src/bundle/runconfig.rs`). The window reads only how
long the system's timer ran, anchor to arrival, on a timer-linked `expect`; it never widens order,
a count, a send's dwell or `within_ms`. The verdict reports every reading, absorbed or not. A dwell
the lane cannot arm at its grain rides the same window, and a difference the window does not cover
refuses the run before it dials.

## 10. The settle contract and postconditions

`crates/pivot-interpreter/src/settle.rs`, `crates/pivot-schema/src/postcondition.rs`.

**After the last flow node the runner always settles**: until every scripted dialog is terminal
and the system reports no active call, bounded by `timing.settle_budget_ms`. **Failing to settle
is always test failure; there is no soft mode.** A dialog is terminal once its transactions are: a
non-2xx final a scripted leg sent holds the run until the ACK or Timer H (`final-unacknowledged`
past it), an INVITE a scripted leg sent holds it until its final (`invite-unanswered` past Timer B
in Calling). The run never waits for a claim; whoever holds the records answers it after the call
(`crates/pivot-interpreter/src/records.rs`, ADR-0060 appendix §4).

**CDR checking is default-on.** A document states a `call-records` expectation or an open reason
token for its absence; a generated one states `{ "absent": "capture-carries-no-cdr" }`. HTTP-body
assertions are not part of the format: the routing adapter is driver-compiled (§4.3) and asserted
at unit level.

## 11. deviations

`crates/pivot-schema/src/deviation.rs`; emission: `crates/pivot-interpreter/src/deviation.rs`,
`preserve.rs`. Every non-compliance an emission must reproduce lives in this one block; there are
no inline tier-1 overrides. `kind` is open so an unknown kind still parses and lints, and an
interpreter that cannot emit a kind fails the run by name.

What the defined kinds do NOT change:

- a `verbatim-emission` step still carries the run configuration's injected headers, appended
  after the stored block; a document header of the same name wins;
- a `cseq-override` BECOMES the leg's sequence number, and later requests continue from it
  (RFC 3261 §12.2.1.1);
- the `preserve` set is closed (`header-order`, `casing`); another token is refused.

A held automatic is no deviation: the hold is the auto step's `delay`.

### 11.1 rfc_violations

`crates/pivot-schema/src/violation.rs`, `crates/pivot-schema/src/bundle/rfc.rs`. A deviation
changes what an emission looks like; an `rfc_violations` entry states a rule a byte-compliant
message breaks by its timing or context, which the replay reproduces by running the flow
unchanged. `rule` is closed over the validator rules with a body (`rfc-rules`), each decided the
same way off a capture (`sipflow --rfc-census`, `crates/sip-pcap/src/rfc/`) and off a run.

**A capture is OPEN, a run is CLOSED.** A capture may have stopped recording before an absence came
due, so an absence is decided only once the recording outlasts the rule's window; a run decides it
at its end.

**A violation is charged to the party that commits it**, and an entry is the statement that
cancels it. A scripted peer's finding never gates as the SUT's: stated by the document on the same
transaction it is CANCELLED; unstated it is a SOURCE FAULT; on a `census-uncovered` document it is
`unjudged`. All three are information. An entry the SUT emits gates. A `sut` entry that `relays` a
party's entry of a relayable rule (`RuleId::RELAYABLE`) states the platform forwarding that
party's violation unchanged, and with the party's entry cancels the SUT's relayed copy
(`caused_by`). A census hit on a hop the case does not carry is stated on the carried hop's copy
of that message (`ts/pipeline/src/captured.ts`). Lint: `ref/violation-*`.

### 11.2 must_fail

`crates/pivot-schema/src/must_fail.rs` (vocabulary), `crates/pivot-interpreter/src/must_fail.rs`
(verdict). Some source non-compliance cannot be reproduced because the source PLATFORM answered
it: where a peer never ACKed a 2xx, this platform ACKs it locally and emits a datagram the capture
never carried. A `must_fail` declaration states the failure the run owes.

- **A document declaring one is a NEGATIVE case.** It passes only by failing as declared, and
  reports `ok-negative`, never `ok`. A declaration the run did not produce is
  `declared-failure-not-produced`.
- **A declaration is an inclusion, not an exhaustive prediction.** Past the divergence, WIRE-class
  failures are carried to the verdict's `tolerated` list; every STRUCTURAL failure (settle, CDR,
  postconditions, gating checks, emission, the SUT breaking a rule) still fails the run. A wire
  failure before the anchor fails it, and carrying is all or nothing.
- **Derived, never hand-guessed.** A declaration is read off the census hit of the detector its
  `derived_from` names; a capture that merely stopped looks identical from inside the document.
  The rule charges the endpoint that owed the answer, so a declaration exists only where the
  scripted peer SENDS the message and this platform answers on replay.
- **The inversion is the verdict's.** No gate, lint rule or recording softens for it; a
  declaration is matched against the recording, never against a failure's wording.
- Where the census charges the capture's own SUT, the generator writes a negative case wherever a
  declaration anchors every charged hit; the `sut-violates:<rule>` refusal stands only on the
  residue (`ts/pipeline/src/caseset.ts`). Lint holds placement (`must-fail/*`).

**A failure does not end a run; being unable to go on does** (§14). This is polarity-free: a
declaration changes nothing about when a script ends.

## 12. media — RESERVED

An open object, deployment-extensible, read by nothing, so a media vocabulary lands additively.
Distinct from `legs[].media`, the per-leg RTP source token.

## 13. Validation

Three layers: the formatter (`pivot-schema fmt --check`, §2.1), the exported schema the TS mirror
is checked against (`pivot-schema schema pivot`), and lint (`pivot-schema lint <file>`, non-zero
on an error). Rule groups and their contracts: `crates/pivot-schema/src/lint.rs` and
`crates/pivot-schema/src/lint/`. CI never diffs pcaps: it runs the scenario's own assertions, and
the confrontation runs as its own pass over the recording.

### 13.1 The generator-subset gate

**A captured document may carry nothing a capture cannot justify**
(`crates/pivot-schema/src/lint/subset.rs`). Refused on `origin: capture`: more than one call,
`background` beyond §5.1's exception, `alt`, `unordered`, `inject`, `optional` (save §6.9's
flagged surplus), `after`, `awaits`, inline and postcondition `checks`, any run-time accessor
(`${num:…}` is not one), and a `cseq-override` relative to a run-time value. Required: `case.source`,
`timing.capture_span_ms`, `observed` on every step.

Readings OF the capture are not inferences and pass: the identity registry, `case.origin_lane`, a
check class, an `rfc_violations` entry and a `must_fail` declaration (the one construct that lets a
captured document state something the capture does not hold, because it pairs a detector's hit
with lane knowledge).

### 13.2 Lane adaptation the generator applies

A generated document states what the REPLAY will see where the difference between the source
platform and this one is a rule, not a judgement. It adds no field: each adaptation rides an open
`case.annotations.flags` kind, and each runs against the unchanged synthesis, so removing one
restores the captured ladder. The ACK to a 2xx is no adaptation: both platforms relay it end to
end (§6.3).

| flag | section |
|---|---|
| `unreliable-provisional-repeat-expanded` | a repeat of a class that rides no ladder (in practice the 100 Trying) kept as steps, §6.9 |
| `ack-count-drawn-from-final` | §6.3 |
| `provisional-expect-surplus-tolerated` | §6.9; the subset gate requires it before any captured `optional` |
| `relayed-provisional-expect-derived` | §6.9 |
| `relay-placed-after-source` | §6.9 |
| `far-side-reinvite-derived`, `far-side-reinvite-not-derived` | §6.9; the gate requires the first before a second step on one coordinate |

**The detector roster.** Absence means "none" (§2.2), so a document without `relay18x` could not
tell "decided transparent" from "never looked". A captured document names its deployment's
detectors (`detector-roster`) and states exactly one outcome per detector: `detected:<d>`,
`detected-none:<d>`, or `detection-unavailable:<d>` when the vantage lacks the messages the
detector reads, which is a decision and not a failure. Lint holds the account
(`crates/pivot-schema/src/lint/annotation_rules.rs`); the interpreter never reads it.

## 14. Interpreter contract

The interpreter is deliberately dumb, with teeth (`crates/pivot-interpreter/src/lib.rs`,
`exec.rs`). **The replay tool never infers what to do.** What the platform must be told lives in
the document and the run configuration the driver compiled from it (§4.3); where the document does
not say, the answer is a refusal naming what is missing, never a guess.

Its whole job:

1. **Bind** each endpoint per its `side` and `binding`.
2. **Sequence** the flow: same-leg list order save §6.7b, §6.7c and §6.7e; cross-leg and
   cross-call order is `after` (`cursor.rs`).
3. **`send`**: emit exactly what `msg` states, plus tier-1 regeneration, run-config injected
   headers (and on a call's dial that call's directive, §4.3), accessor substitution and §8.3's
   two body rewrites. Nothing else (`render.rs`). It never judges one message a repeat of
   another, and expands a count only for a class RFC 3261/3262 paces or one item 7 draws.
4. **`expect`**: gate on op, leg alignment, discriminator and budget (§6.8), then `check`, then
   inline `checks` (`gate.rs`). A datagram no armed expect matched is charged to the expect of
   ITS transaction, read off the recording; a final so charged retires that expect; a final may
   stand in for its INVITE's `100` (`trying-omitted`); §6.7d is the one other status an expect
   takes (`progress.rs`, `exec/omission.rs`).
5. **Lane scoping** (§9.1): one comparison, no vocabulary, no third status.
6. **`rfc_violations`** (§11.1): listed in the verdict; a scripted peer's cancels, the SUT's
   gates.
7. **`auto` steps** (§6.3): the stack composes them, the stored content rides them, an auto
   step's `cseq` is never read, an auto ACK's `retransmits` is drawn, and a count on another
   unpaced class is refused before the wire (`retransmits/unpaced-provisional`).
8. **`background`** (§5.1): answer, record, never move the cursor, yield to an open frontier
   expect.
9. **`alt`**: commit on the first discriminating message; never backtrack.
10. **`inject`** (§6.6): hand over once due, complete on the answer, never execute.
11. **Dwell**: sleep `delay.ms` on a `send`, compress only where `compressible`; on a
    timer-linked `expect`, measure and report against the run's window (§9.2).
12. **Record, always**: a verbatim per-leg recording of every datagram as BYTES, in wire order,
    with its body layout, in every mode and on every lane
    (`crates/pivot-schema/src/bundle/recording.rs`, `crates/pivot-interpreter/src/recording.rs`).
13. **Settle** (§10), then evaluate `postconditions`.

**A failure does not end a run; being unable to GO ON does.** The run records every failure and
keeps following its flow. The SCRIPT ends on a required `expect` whose budget ran out, or on an
arrival that leaves the tail uncomposable (`crates/pivot-interpreter/src/progress.rs`). The rule
is polarity-free. **The script ends; the call is closed generically**: every scripted endpoint ends
what it holds by the RFC's own rules, read off its recording and composed by its own stack, inside
the settle budget (`crates/pivot-interpreter/src/close.rs`). Abandoned steps raise no
`flow-incomplete`; the verdict's `abandoned` section names them.

What the interpreter never does: read `calls` beyond leg sequencing and per-call directive
placement, read `case.annotations`, `case.requires` or `observed`, choose a matching strictness,
infer that a message was minted rather than relayed, re-split a multipart body, or hold callflow
logic.

**Compile once, run many.** A document compiles to an immutable plan; a run instance is that plan
plus identity substitution, with no per-call parsing (`plan.rs`, `instance.rs`).

## 15. Marked fields

Everything in this document is frozen except the following.

| item | status | reason |
|---|---|---|
| `must_fail` vocabulary | **MINIMAL** | §11.2. `unexpected-ack`, `unexpected-prack`, `unexpected-cancel`. It grows one prediction at a time, each with the derivation that decides it |
| `media` | **RESERVED** | §12 |
| match-less retry chains | **UNEXERCISED** | `calls[].attempts` expresses a retry chain ordered by a match-less rule; nothing has produced one |
| body-registry deployment overlay | **NOT MODELLED** | §8.2. A deployment handler that REWRITES a body has no field on a `body` or part to name itself with |
