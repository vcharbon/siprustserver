# PCAP2TEST_PIVOT_V3.md — pivot scenario format, v3 (NORMATIVE)

`pivot_version: 3`. Supersedes the v2 draft, which was removed once nothing
cited it; it remains in git history. There is no v2 compatibility: the corpus is
pcap-generated and regenerates.

The pivot is the declarative artifact between a capture and a replayable test.
v3 makes it **one format for two things**: a replayed capture, and a test a
human wrote. A captured call is the degenerate straight-line case; an authored
test uses the extension constructs; one interpreter runs both. What keeps that
safe is a line drawn from both sides — the generator emits a strict subset, and
lint refuses a captured document that carries anything outside it.

Design rule, unchanged and still deciding every argument below: **smart
compiler, dumb interpreter.** A corner case is compiled into explicit fields by
the generator, never inferred at replay time. The named anti-example is the
v0.1 interpreter's keepalive elision: callflow knowledge inside an interpreter
is a design failure of the format, and `background` (§5.1) is where that
particular knowledge now lives — as document data.

Rationale and the design record live with the consumer that authored the
format. This file states the contract and nothing else.

> **AMENDED 2026-08-22.** Lane-scoped assert classes (§9.1), `rfc_violations`
> (§11.1), the in-dialog final marker and the `cause` citation rule (§4.1,
> §6.1), the optional-expect release (§6.5, §14), and two removals —
> `calls[].setup_deadline_ms`, and REFER with `Replaces` (the `has-replaces`
> claim). See §0.1.

**Schema ownership.** The upstream crate `pivot-schema` owns this contract: its
serde structs and their schemars JSON Schema export (`pivot-schema schema
pivot`) are the source of truth, and this document is their prose companion.
Where the two disagree the structs govern. Rust owns every wire contract; CI
fixtures check the TS mirrors (`@sip/contracts`'s `pnpm test`, against the
committed `crates/pivot-schema/tests/fixtures`).

## 0. What changed from v2

A v2 reader looking for a field that moved:

| v2 | v3 |
|---|---|
| `flow[].step: <int>` | `flow[].id: "<string>"`. Every reference is id-based: `delay.from: "step:<id>"`, `after`, `deviations[].step`, `defect.marker.step` |
| `routing.attempts` | `calls[].attempts`. `relay18x` moved onto the call with it, and the `routing` container is gone. v2's `setup_deadline_ms` has no v3 counterpart (§0.1) |
| — | `calls[]`: a document may play several calls at once |
| cross-leg order derived from `routing.attempts` | `after: ["<step-id>"]`, stated |
| resource name `resources/s07_uac1_0.sdp` | `resources/uac1_3_0.sdp` — actor, ordinal within the case, part index. Step-index-free (friction H1) |
| — | `case.origin`: `capture` or `authored`, the subset gate's discriminator |
| `case.source` required | required for `origin: capture`, absent otherwise |
| — | `case.requires`: informative capability tokens |
| identity embedded on the attempt | `identities`: a document-level registry; `attempts[].callee` and `actors[].identity` name an entry, and `${num:<name>:<form>}` composes number-bearing headers from it (§8.5) |
| transfer legs opened a new `branch` and read as a fork | `attempts[].joined_by: {kind, step}`: what ADDED the leg to a running call, independent of the exit-side `cause` (§4.1) |
| a deviation pointed at a step | plus `deviations[].header`: which header a content-level malformation breaks (§11) |
| — | `actors[].background`: the traffic an actor answers outside the flow |
| — | `alt` / `unordered` flow nodes, `optional` on an expect |
| — | `inject` flow node (schema and shape only; no execution semantics) |
| — | `checks` on an expect, and a `postconditions` block, in one check vocabulary |
| — | accessors: `${leg:…}` and `${step:…}`, with `{ from, delta }` arithmetic |
| deviation kinds: `verbatim-emission` | plus `cseq-override`, `suppress-auto`, `raw-order` |
| `timing.capture_span_ms` required | required for `origin: capture`, absent otherwise |
| — | `timing.settle_budget_ms`: the settle-phase ceiling |
| — | `media`: reserved, deployment-extensible |

Everything v2 states that is not in this table is unchanged in v3.

### 0.1 Amendments

A format change is user-authorized, and lands in one batch across this
document, the `pivot-schema` structs and the generator. Every batch gets an
entry here, and the entry is the index: what changed, and where the contract
now reads.

**2026-09-15 — the settle waits on the scripted legs' own INVITE server
transactions.** A non-2xx final a scripted leg sent to an INVITE holds a server
transaction in Completed until the ACK the system owes on the INVITE's branch
or Timer H (RFC 3261 §17.1.1.3, §17.2.1). The settle read the flow and the
system's call count only, so a run closed in the instant such a final went
out and the ACK landed after the recording; a system that never ACKed passed
identically. No document field changes; the verdict gains one failure.

| change | where |
|---|---|
| the settle floor: an un-ACKed non-2xx INVITE final a scripted leg sent keeps the run open, bounded by Timer H from the final's first emission, whoever composed the final — a step, the unscripted answer, the generic close. A budget that runs out names it (`leg B: 487 to INVITE CSeq 2 awaits its ACK`) | §10, `pivot-interpreter` |
| past Timer H the transaction is gone and the ACK never came: the run settles and states `final-unacknowledged` (`leg`, `status`, `cseq`), a structural failure a declared divergence never carries | §10, `pivot-schema` verdict |
| the ACK that ends the wait is recorded as the transaction's own closer (`absorbed: the §17.1.1.3 ACK …`), never as `datagram-after-flow` | §10, `pivot-interpreter` |

**2026-09-14 — an expected session description is asserted by CONTENT, the
lane-owned fields masked where the run rebooked them.** An expected SDP was a
declared shape (`sdp-present`) and nothing read its lines, so a description the
system altered — a payload dropped from the `m=` list, an `a=fmtp` lost, a
direction changed, an answer dropped from an ACK — replayed as transparent. The
generator now stores every expected SDP as a resource compared as a session
description; the shape stays for an authored document that asserts presence
only.

| change | where |
|---|---|
| `BodyCompare` gains `sdp`: session section then media sections by position, lines per section as a multiset (attribute order erased), `o=` sess-id and sess-version masked always, and every field the expect's own `rewrite` tokens name — exactly what the render writes: `c=addr` the address of a `c=IN IP4` line, `m=port` the non-zero port of an `m=` line with its `/count` kept, `a=rtcp` never — masked where the run's media plane REBOOKED it; on a verbatim run the tokens mask nothing and two descriptions the structure cannot tell apart must be the same bytes (one `document:bytes` record otherwise). Refused on a body whose stated content type is not `application/sdp` by `body/compare-sdp-type` | §8.3, `pivot-schema`, `@sip/contracts` |
| the generator stores an expected SDP as `{ ref, rewrite, compare: "sdp" }` plus its resource file, under the expect-side name `resources/<actor>_r<n>_0.sdp`; multipart stays a shape | §8.3, generator |
| the interpreter gates a `compare: sdp` resource on presence AND media type, exactly as the `sdp-present` shape gates; the content is the confrontation's | §6.3, §8.3, `pivot-interpreter` |
| the confrontation states one `body` record per differing line key, `body:sdp:<section>:<line>:<scope>` (sections `session`, `m<i>`), a side that is no session description as one `body:sdp:document:sdp:<scope>` record, a media section on one side only as one `body:sdp:m<i>:section:<scope>` record, two descriptions a verbatim run carried as different bytes with the structure equal as one `body:sdp:document:bytes:<scope>` record — every line verbatim, each side one element per line as a header record carries one per value; the driver reads the run's media mode off the bundle and hands it to the confrontation | the pipeline's `confront`, `sdpfold`, the driver |

**2026-09-13 — an expected body is asserted by CONTENT, and the confrontation
states the difference.** A single body on an expect was a declared shape and
nothing else, so a frozen text body the capture carried — a control document,
a sipfrag — was not stored on the expect side and a replay that received any
document at all read as transparent: the confrontation diffed headers and
never read a body. The registry now decides the expect side as it decides the
send side. SDP and multipart stay shapes, absence stays its own claim, a
binary payload stays undeclared, and every other frozen text body is stored as
a resource beside the send-side files and held against the received body
after the run.

| change | where |
|---|---|
| `ResourceBody` gains `compare` (`exact`, the meaning of absence, or `xml`: declaration dropped, whitespace-only text between tags removed, ends trimmed, nothing else). Refused on a send by `body/compare-on-send` | §8.3, `pivot-schema`, `@sip/contracts` |
| the generator stores a frozen TEXT body on an expect as `{ ref, mode: "frozen", content-type }` plus its resource file, under the expect-side name `resources/<actor>_r<n>_<part>.<ext>` — its own counter, so nothing a send stored is renamed | §8.3, §8.4, generator |
| the interpreter keeps gating PRESENCE for a resource body on an expect, under `check: assert` alone; the content is the confrontation's, read off the recording, and on a `check: record` step its record is the only statement of the body. `auto/body-not-composable` reads sends only: an expect composes nothing on any class | §6.3, §8.3, `pivot-schema`, `pivot-interpreter` |
| the confrontation gains `body` records: `body:<type/subtype>:<scope>`, the expected and received texts one a side, both verbatim; a reception carrying no body confronts as the empty text; the driver supplies every expect-side resource from the case directory and a ref it did not supply is a driver error | `ConfrontationRecord.kind`, the pipeline's `confront`, the driver |

**2026-08-30 — an expect-side ladder states its FACTS and the interpreter
excuses nothing.** Ticket 211. §6.9 splits a ladder — the count is the
document's, the pacing is RFC 3261's per message class — and on a `send` step
that split closes, because the scripted peer paces itself by the document's own
gaps. On an `expect` step it cannot: the emitter is the SUT on its own T1 while
the count was read off a platform on another one, so the two counts are one
dwell measured against two ladders and the cut is faithful either way. The
interpreter had answered that with a 30 ms wall-clock margin
(`ladder-overtaken-by-answer`), which derived from nothing and missed by 25 ms
on `capture_181953` s45. The margin is gone; the note carries the facts instead
and a post-run tolerance re-counts them.

| change | where |
|---|---|
| `RetransmitNote` gains `side` (`send` / `expect`), `intervals_ms` (the document's own gaps for the step), `dwell_us` (the claimed datagram to the closer that ended the ladder) and `rfc_rungs` (the rungs an RFC-paced ladder of the message's class puts inside that dwell); `blessed` is removed | §6.9, `pivot-schema`, `@sip/contracts` |
| `retransmit-count-mismatch` gates unconditionally: the interpreter counts the ladder and excuses no count, on any lane | §6.9, `pivot-interpreter` |
| the tolerance is `ladder-recounted-under-rfc-pacing`, a second bless premise on the post-run reclassifier: expect-side, one stated gap per declared rung, the captured pacing putting the DECLARED rungs in the dwell and the RFC ladder the OBSERVED ones — all four or the failure stands | the downstream rules package (`ladder-recount.ts`), audited by issue 163 |

**2026-08-26 — `auto` is a composition marker, not a storage policy.** Ruling
of the design record. The
closed field list on an auto step was the ONE departure from the three-tier
model in the pipeline, and it left an ACK's frozen headers and the delayed
offer's ANSWER with no home: measured over 400 captures / 8910 steps, scripted
steps dropped 0 headers and 0 bodies while auto steps dropped 993 and 99. The
justification written for the list — "a scripted copy emits a second automatic"
— was false in this interpreter, which emits only from `emit(&step)`. Every
delayed-offer ACK in the corpus therefore asserted a preservation the document
did not perform.

| change | where |
|---|---|
| the closed field list is GONE. An auto step goes through the same three-tier build as any other: tier-1 omitted, tier-2 as refs, everything else frozen in wire order, and the body where the class can carry one. Lint's `auto/carries-content` is deleted | §6.3, §8, generator |
| the marker states what it always meant underneath: the stack derives R-URI, Route, Via and CSeq from the TRANSACTION that obliged the message, not from dialog state. `cseq` stays, as the label of that captured transaction | §6.3, §14 item 7 |
| three classes carry a stored body — the ACK to a 2xx (RFC 3261 §13.2.1), PRACK and its 2xx (RFC 3262 §5). Refused on 100 Trying and on the §17.1.1.3 non-2xx ACK, by lint (`auto/body-not-composable`) and by the generator, which drops the payload with an `automatic-body-dropped` flag rather than storing it where nothing emits it. A declared SHAPE is not a stored body and rides any class | §6.3, `pivot-schema`, generator |
| a `verbatim-emission` naming an auto step COMPILES: there is a stored block to preserve. `PlanError::PreservesAutomaticStep` is deleted | §11, interpreter |

Consequence, stated because it must be measured and not absorbed: once the
harness DRIVES those headers, every relay claim issue 65 declined on
`request:ACK:in-dialog`, `request:PRACK:in-dialog` and `response:200:PRACK`
becomes decidable again. The capability set on an ACK or a PRACK is inert
(RFC 3261 §20.5, §12.2 — an ACK is no target-refresh request, and it draws no
response to negotiate in), so the delta registry accepts it there and NOWHERE
else; the charging and network context on the same message stays a finding.

**2026-08-26 — an auto ACK's transaction is LEG STATE's, not the token's.**
Ruling of the design record. No
field shape changes; the CONTRACT does. The interpreter honoured §8's "`cseq` is
a pairing identity token, NEVER replayed" on the auto EXPECT path and broke it on
the auto SEND path, resolving an auto ACK's target INVITE by the captured number.
A captured number is the authoring platform's and names no transaction in a run
the stack numbers itself.

| change | where |
|---|---|
| an auto ACK acknowledges the final that answered the INVITE its leg has OUTSTANDING — the resolution `Stack::ack_for` already composes against and the generic close already performs. RFC 3261 §14.1 leaves one INVITE outstanding per dialog, so it is unambiguous | §6.3, §14 item 7, interpreter |
| `msg.cseq` on an auto step is read at run time for ONE thing: the marker that scopes an auto ACK's DRAWN `retransmits` to a transaction. It is never resolved against | §6.3, `pivot-schema` |

**2026-08-28 — the source's LATE CANCEL joins both vocabularies.** A source
platform that CANCELled an INVITE transaction it had already taken — and ACKed —
a final response on sent a CANCEL that reached no transaction at all (481). This
platform CANCELs while the transaction is in flight (RFC 3261 §9.1), so the
capture's own CANCEL step can never be satisfied where the capture put it and
the platform's own arrives ahead of it. The ordering is not a race to relax: a
document that accepted either order would go green for a SUT that cancels a
completed transaction, which is the very violation the source's 481 records.

| change | where |
|---|---|
| `rfc_violations[].rule` gains `no-cancel-after-final` (RFC 3261 §9.1 / §17.1.1.2), with its detector in `crates/rfc-rules/src/rules/cancel.rs`. Conservative: the emitter's OWN ACK for that final is what separates it from a crossing | §11.1 |
| `must_fail[].failure` gains `unexpected-cancel`. Anchored on the FINAL RESPONSE the source's CANCEL arrived behind — the transaction end, which is also the dialog the recording matches the platform's CANCEL against; derived from `no-cancel-after-final` | §2, §11.2 |
| `pivot-schema lint` holds the new shape: `must-fail/anchor-not-a-final-send`, `must-fail/anchor-has-no-late-cancel`, `must-fail/anchor-already-cancelled` | §11.2, §13 |
| `no-cancel-after-final` joins the SUT-invalid rules, deferred like the other two: withdrawn where the case declares every charged coordinate | generator |

**2026-08-25 — SUT-invalid captures are NEGATIVE cases; `unexpected-prack`
joins the vocabulary; every cannot-go-on abort closes.** Ruling of the design
record. This RETIRES the
2026-08-23 SUT-invalid EXCLUSION: a call group whose own system under test the
census charges is no longer refused — the generator derives the `must_fail`
that predicts the replay's divergence and writes the case as a NEGATIVE one,
proving both that failure detection works and that the bad behavior is gone.
No auto-repair is attempted: there is no way to guess how such a call would
have ended. The ONLY ground for not generating a case from a capture is an
INCOMPLETE source call (`source-call-incomplete`, Q51 (b)); the census refusal
survives solely as a DEFERRED guard on the residue no declaration can anchor.

| change | where |
|---|---|
| `must_fail[].failure` gains `unexpected-prack` (RFC 3262 §4): this platform PRACKs the reliable provisional the scripted peer sends, where the source never did. Anchored on that provisional's send; derived from `unacked-reliable-provisional` | §2, §11.2 |
| `pivot-schema lint` holds the new shape: `must-fail/anchor-not-a-reliable-provisional-send`, `must-fail/anchor-already-pracked` | §11.2, §13 |
| both SUT-invalid census refusals (`sut-violates:<rule>`) are DEFERRED: withdrawn where the case declares every charged coordinate, standing only on the undeclarable residue. A charged coordinate carries its RULE, so one rule's anchor never withdraws another's refusal | generator |
| census-sync settles a hit on a REFUSED case in its own `CASE REFUSED` block instead of writing an entry no case will ever carry; decisions are recomputed each run, so a later re-cut re-surfaces the hit | census-sync |
| every abort site where the run truly cannot go on arms the generic close (`Abandoned.leg` now optional for the legless sites); a gating inline check no longer aborts at all — the finding fails the verdict and the flow keeps walking | interpreter (Q53) |

**2026-08-23 — `must_fail`: a document declares the failure its run MUST
produce.** Ruling of the third evidence round. A source whose
peer withheld an ACK recorded a ladder this platform does not run, and the goal
is to replay reality rather than something like it: the run fails, and the
document states in advance exactly how. A new top-level field is a format
change, so it lands here.

| change | where |
|---|---|
| `must_fail[]`: `failure` (CLOSED, one member `unexpected-ack`), `step` (the anchor) and `derived_from` (the §11.1 rule the source broke). Declaring any makes the document a NEGATIVE case, which passes only by failing exactly as declared | §2, §11.2 |
| `pivot-schema lint` holds the placement: `ref/must-fail-step-unknown`, `must-fail/duplicate`, `must-fail/anchor-not-a-2xx-send`, `must-fail/anchor-already-acked` | §11.2, §13 |
| the subset gate lets a captured document carry it: a declaration pairs a detector's hit with the lane knowledge §13.2 already applies, and it is the one construct by which a captured document may state something the capture does not hold | §13.1 |
| the generator DERIVES it from the census hit, at the seam that stamps `rfc_violations`, and never from the shape of the flow: only where the scripted peer SENDS the unACKed 2xx does this platform's own local ACK arrive where nothing expects it | generator |

The verdict inversion is NOT in this batch: a run does not yet read the
declaration, so a negative case's run reports the plain failure. §15 marked it,
and the entry below closes it. (The SUT-invalid exclusion that stood beside
this batch is RETIRED by the 2026-08-25 entry above: such captures generate as
negative cases now.)

**2026-08-23 — `must_fail`: the verdict inversion, `ok-negative`.** The
behaviour half of the entry above, so the declaration a document carries is now
read by the run that carries it. No field changes; the run bundle gains a
`must_fail` section and a third status, and §15's `DECLARED ONLY` mark is gone.

| change | where |
|---|---|
| a run whose every declaration was produced, and nothing else failed, reports `ok-negative`; a declaration the run did not produce is the failure `declared-failure-not-produced`; any other failure beside the declared ones fails the run | §11.2, §15 |
| the matched failure is listed under the verdict's `must_fail`, carrying the gate's own failure verbatim, as a downgraded check is listed under `informative` — `failures` stays the list of what went wrong | §11.2 |
| `unexpected-ack` is matched against the RECORDING: an ACK on the anchor's own leg carrying the Call-ID, INVITE CSeq and To-tag of the 2xx that step emitted, claimed by no flow step | §11.2 |
| the run keeps replaying past a declared datagram, so a negative case still tears its call down, settles and evaluates its postconditions. The gate, the lint rules and the recording are untouched | §10, §11.2 |

**2026-08-23 — `must_fail`: the declaration INCLUDES, and the derivation goes
first.** Ruling Q47 (b) of the collision round. The two entries
above read "the declared failure and nothing else", which no corpus negative can
meet: replaying a capture the replay is known to diverge from makes the tail
diverge too. No field changes; the run bundle gains a `tolerated` list, and the
generator stops refusing a case it can declare.

| change | where |
|---|---|
| a run that produced every declaration CARRIES the WIRE failures beside them — unexpected/unmatched/after-flow datagrams, expect timeouts, retransmit counts, timing tolerances — at or after the declared divergence, and still fails on every STRUCTURAL one: settle, CDR, postconditions, gating checks, emission and lane failures, a SUT violation (§11.1) | §11.2 |
| a carried divergence MOVES to the verdict's `tolerated` list, never dropped; carrying is all or nothing, and a wire failure before the divergence still fails the run | §11.2 |
| where the census charges the capture's own SUT with the withheld ACK, the DERIVATION decides first: the case is generated as a negative one wherever it anchors a declaration for every charged hit, and refused by category only where it cannot. The unPRACKed-provisional rule still refuses outright | §11.2, §0.1 (2026-08-23) |

**2026-08-24 — what ends a script is being unable to GO ON, on every
document.** Ruling Q52, correcting the
trigger the entry below states. Ending early has nothing to do with whether a
document is positive or negative: it is about whether the run can compose its
next message. No field changes; what moves is where the rule is written, since it
is run semantics and not a property of a declaration.

| change | where |
|---|---|
| a failure the run can go on past is RECORDED and the flow CONTINUES — a datagram nothing expected, one no armed expect matched while the expected message can still arrive. The declared ACK is one of them, so a negative case's scripted teardown runs to its end | §11.2, §14 |
| what ends the script is a required `expect` whose budget ran out, or an arrival that leaves an armed expect UNSATISFIABLE: a FINAL response on the transaction the expect is gated on, with a status it cannot match (RFC 3261 §17.1). Alternatives armed together must ALL be contradicted; an `optional` expect never blocks | §14 |
| the rule and the generic close are POLARITY-FREE and move to §14, where run semantics live. A positive run that cannot go on is abandoned, closed and settled exactly like a negative one — and still fails. §11.2 keeps only what a declaration adds | §10, §11.2, §14 |

**2026-08-23 — `must_fail`: the first delta ends the SCRIPT, and the call is
closed generically.** Ruling Q50. The two entries
above have a negative run keep replaying past its divergence, which scripts a
conversation neither party is having; and the tail's own aborting deltas were
piling a settle failure on top of the declaration anyway. No field changes; the
run bundle gains an `abandoned` section and a second way for a declaration to be
observed.

| change | where |
|---|---|
| on a NEGATIVE document the first aborting delta — a gate-refused arrival, an expect timeout — ends the script. The failure and the recording are untouched; what stops is the flow. A positive document is byte-identical to before | §11.2 |
| the scripted endpoints then close GENERICALLY: answer what they took, ACK what they took, BYE what they opened, CANCEL an unanswered INVITE they sent — and never start a teardown the far side owes. Bounded by `timing.settle_budget_ms`, and an obligation still open keeps the run from settling | §10, §11.2 |
| `flow-incomplete` does not fire for the abandoned steps. `completed_steps` stays truthful and the verdict's `abandoned` states the leg and step the script stopped at, the nodes it never ran, and every act the close emitted | §11.2 |
| a declaration that arrived after the script ended is observed by the RECORDING, listed as `recorded` beside the `observed` a gate's failure fills. The CDR expectation still gates: the call is billed like any other | §10, §11.2 |

**2026-08-23 — `flow[].confirms_dialog` names the ACK that completes the
handshake.** Ruling of the third evidence round. `in_dialog` says an
ACK runs inside an established dialog; it does not say whether that ACK is the
one that established it. A new field is a format change, so it lands here.

| change | where |
|---|---|
| `flow[].confirms_dialog`: `true` on the ACK answering a leg's dialog-creating final, absent everywhere else. It co-occurs with `in_dialog` — that ACK is strictly after the final — and with `early` where the ACK names its fork; under forking each answered fork's own ACK carries it | §6.1 |
| a re-INVITE's ACK stays plain `in_dialog`, and an ACK to a non-2xx final (§17.1.1.3) carries neither marker | §6.1 |
| `pivot-schema lint` enforces the placement: `in-dialog/confirm-missing`, `in-dialog/confirm-not-ack`, `in-dialog/confirm-outside-dialog`, `in-dialog/confirm-not-the-answer`, `in-dialog/confirm-duplicate`. Requiredness is the MARKER's, not the message's: a flow carrying no ACK after its final declares no confirming ACK, and `no-ack-to-dialog-creating-2xx` is what speaks to that | §6.1, §13 |
| the generator stamps it in the pass that stamps `in_dialog`, so its own output passes that lint | generator |

**2026-08-23 — a `content-type` is stored VERBATIM, and the boundary is the ONLY
thing emission regenerates.** Ruling 4 of the second evidence round: the
payload is
properly managed, binary-identical outside the SDP rewrite. No field is added or
removed; what changes is what a stored `content-type` MEANS and, on one arm,
when it is written — which is exactly what this index is for.

| change | where |
|---|---|
| every `content-type` — a part's, a single body's, the multipart container's — is the wire value verbatim, MIME parameters included; the body registry matches on the bare type, so handling is unaffected | §8.3 |
| `body.content-type` may now ride on a REWRITTEN body: bare `application/sdp` stays derived at render, a parameterized SDP type is stated and emitted | §8.3 |
| the container is held with its `boundary` stripped and NO other parameter, and emission re-derives only that | §8.3 |
| the generator stored a part's bare media type and dropped its parameters; it stores the wire value | generator |

**2026-08-23 — `no-ack-to-dialog-creating-2xx` joins the closed vocabulary.**
Headline ruling of the second evidence round. A closed enum
growing IS a format change, so it lands here. (The SUT-side EXCLUSION this
batch also introduced is RETIRED by the 2026-08-25 entry above.)

| change | where |
|---|---|
| `rfc_violations[].rule` gains `no-ack-to-dialog-creating-2xx` (RFC 3261 §13.2.2.4), with its detector in `crates/sip-pcap/src/rfc/ack.rs` and the report field it carries | §11.1 |

**2026-08-23 — `flow[].in_dialog` is TOTAL.** Ruling Q31. No field shape
changes; the CONTRACT does, which is what this index is for — a document that
marked only the finals §4.1 reads is now a lint error, so the batch lands across
this document, `pivot-schema`'s lint and the generator exactly as an amendment
does.

| change | where |
|---|---|
| the marker is stated on EVERY step after the leg's dialog-creating final and on none at or before it, with the CANCEL and `alt`-branch exclusions; `early` covers the unconfirmed dialog and the two never compete | §6.1 |
| `pivot-schema lint` enforces the totality per leg: `in-dialog/missing`, `in-dialog/outside-dialog` | §6.1, §13 |
| the generator stamps it mechanically, so its own output passes that lint | generator |

**2026-08-22 — lane scoping, RFC violations, in-dialog finals, optional
release.** Rulings of the design record.

| change | where |
|---|---|
| `case.origin_lane`, and check CLASSES on a check and on a frozen header, with the one downgrade rule a run applies | §3, §9.1 |
| `rfc_violations`: a closed-enum list of RFC rules a message the flow already carries breaks, with an anchor and an emitter | §2, §11.1 |
| `flow[].in_dialog`, and the rule that a leg's `cause` may cite only a dialog-creating final or an actual closer. New `cause` member `closed:bye` | §4.1, §6.1 |
| an `optional` expect whose own budget expires is RELEASED, stated normatively | §6.5, §14 |
| the replay tool never infers what to do: a generated document carries explicit instructions | §14 |

What deliberately did NOT land, and waits on the per-endpoint violation census:
an `allowed` flag on a violation, generator auto-plumbing of `rfc_violations`,
any flows-schema change, and any auto-exclusion rule keyed on a violation.

**2026-08-22 — removals: `setup_deadline_ms` and REFER with `Replaces`.**
Rulings of the design record.

| change | where |
|---|---|
| `calls[].setup_deadline_ms` is REMOVED — one concept, one spelling, and the wire only ever shows the cancel timing, which `attempts[].no_answer_ms` already states. §4.3 (the PROVISIONAL section) and the §15 row are gone with it, and the per-lane compilation contract renumbers §4.4 → §4.3 | §0, §4, §4.3, §15 |
| REFER with `Replaces` is OUT OF SCOPE. The `has-replaces` claim discriminator is REMOVED from `actors[].claim.by`, leaving `ruri-pos` and `arrival-order`; the §15 `replaces`-correlated row is gone, and the authored attended-transfer fixture is replaced by `authored-consultation-refer.v3.json`, which carries the same authored-only constructs with a plain `Refer-To` | §5, §15, fixture set |
| the generator EXCLUDES a call group carrying that mechanism instead of cutting a case from it, loudly: reason token `refer-replaces-out-of-scope` on stdout, the evidence datagram on stderr, and both in `<out>/<capture>/excluded.json` (`testkit/ts/rules/src/exclusions.ts`). Zero groups in the 10 182-document corpus match | generator |

`setup_deadline_ms` may return by amendment the day a per-call setup budget
becomes observable as a distinct fact rather than a second spelling of the
no-answer dwell.

**2026-08-22 — lane vocabulary: `fake` → an explicit lane name.** The
`case.lanes` key `fake` is renamed to a named lane, matching the `origin_lane`
vocabulary —
§9.1's downgrade rule compares the run's lane name against `case.origin_lane`
by string equality, so one lane spelled two ways would downgrade classified
checks on the origin's own lane. Renamed in the generator, this document's
examples, drafts and staged fixtures now; the bulk corpus picks it up at
regeneration (issue 07).

**2026-08-22 — forked early dialogs: an accessor namespace, and UPDATE inside
one.** Rulings Q29 and Q38.

| change | where |
|---|---|
| `${early:<id>.tag}` and `${early:<id>.rseq}`: a fourth accessor namespace reading ONE fork of a forking leg, named by the `early` id its steps carry. A leg ringing two forks has two To-tags and two RSeq spaces (RFC 3261 §12.1.1, RFC 3262 §3), which a single-valued leg accessor cannot state; `${leg:<id>.rseq}` keeps its last-sighted reading unchanged. Lint refuses an id no step declares, and an id two legs declare | §6.1, §8.1 |
| an UPDATE runs inside an EARLY dialog (RFC 3311 §5.1), reversing the rung-2 refuse-until-confirmed reading. `early` on an UPDATE or a PRACK `send` names the fork the request RIDES; an UPDATE naming no fork on a leg ringing exactly one rides that one; several forks and no name is a refusal | §6.1 |

The authored `forked-100rel-prack-per-early-dialog` draft is the exercise: its
"each PRACK rides its own fork" assertion was an `RAck` regex proxy and now
reads the two forks' own tags and RSeqs.

**2026-08-22 — verbatim body fidelity: a part's own entity block.** Ruling Q35.

| change | where |
|---|---|
| `body.parts[]` gains `content-id`, the part's OWN `Content-ID` value (distinct from `cid-linked`, which is the LINK record), and `headers`, its remaining entity headers verbatim in wire order (RFC 2045 §3). Extraction reads both off the wire and the generator states them where the capture carried them | §8.3 |
| a body is emitted as the document holds it — payloads, ids and entity headers byte-exact — and SDP, direct or as a part, is the ONE content the tool rewrites, because replay rebooks addresses and ports | §8.3 |
| a `cid-linked` part stating a `content-id` EMITS under it; one stating none is refused by name, so a document written before the id was stored fails loudly rather than emitting a stranded reference (RFC 5621 §3) | §8.3, §14 |

The flows schema is NOT bumped: `msgs[].body.parts[].headers` is additive and
omitted when empty, and schema 5's rule is a bump on a BREAKING change to the
emitted shape. A flows document produced before this batch states no part
headers because its producer read none — the corpus picks them up at
regeneration (issue 07).

**2026-08-22 — the ACK ladder: drawn, and collapsed once.** Ruling Q34.

| change | where |
|---|---|
| `retransmits` on an auto ACK step means ONE ACK per repeat of the final its `cseq` names (RFC 3261 §13.2.2.4), drawn by the wire and never paced — an ACK rides no retransmission timer. Copies of the final that arrive while the ACK is held are each still owed one. A count on a scripted ACK, or on one naming no `cseq`, is refused by the interpreter and by lint (`retransmits/scripted-ack`) | §6.3, §6.9, §14 |
| the generator collapses a repeat onto the step it repeats using the flows document's `repeat_of`, not `retx`. **Amended 2026-08-26 (issue 81/82): the fresh-branch re-ACK no longer collapses** — a repeat is the same datagram byte for byte, and those ACKs are not, so H8's ladder is encoded once because it is one step per emission, not because a count absorbs it. `retx` is the same-branch half of that one relation, not a second criterion, and both are bounded by the transaction envelope and by byte identity (§6.9) — which makes the two halves coincide. A document whose producer computed no `repeat_of` keeps the `retx` collapse and carries a `retransmit-collapse-legacy` flag | generator, §6.9 |

The drawn shape is exercised end to end by the authored
`bc-rc-drawn-ack-per-2xx` draft and its run bundle. The `samples/` documents and
the bulk corpus still show the pre-collapse encoding; they pick it up at
regeneration (issue 07).

**2026-08-22 — ratified working readings.** Ruling Q38 accepted these as
readings of the format, amendable like anything else here. Each is now stated
where its section already discusses the matter.

| reading | where |
|---|---|
| `early` on an EXPECT gates the message's To-tag against the named fork's | §6.1 |
| an auto PRACK acknowledges outstanding reliable provisionals oldest-first: N provisionals, N PRACKs, in arrival order | §6.3 |
| a `${leg:…}`-composed `Refer-To` has no end-to-end consumer while REFER with `Replaces` is out of scope; the accessors stay defined | §8.1 |
| a lowered route plan states branch 0's chain only; a joined branch is dialled by whatever joined it | §4.3 |
| the retransmit count is the document's and the ladder pacing is RFC 3261's per message class; a repeated provisional is no §17.2 duplicate, and it is a §6.9 retransmission to COUNT on an `expect` — on a `send` a count states a ladder, so it is stated only where the class is paced or the copies are drawn. A repeat is one only INSIDE the transaction envelope and only where it is the same datagram byte for byte; past either bound the message is a re-emission and no count exists to state (§6.9) | §6.9 |
| `verbatim-emission` still carries lane-injected headers, appended after the stored block; a `cseq-override` becomes the leg's sequence number for subsequent requests; the honoured `preserve` set is closed — `header-order`, `casing` | §11 |
| a `case.requires` token may imply scene PARAMETERS the document does not carry: the runner pairs scenario × scene | §3.2 |

One work item rode with them: the `verbatim-emission`-on-an-auto-step refusal,
enforced by the interpreter's plan, is now mirrored in lint as
`deviation/verbatim-emission-auto`, so a generator learns it at lint time.

**2026-08-23 — the `rfc_violations` rule vocabulary grows a second member.**
Ruling Q30.

| change | where |
|---|---|
| `unacked-reliable-provisional` joins the closed `rule` enum, with its detector: RFC 3262 §4, a UAC that took a reliable provisional and never PRACKed it | §11.1 |

A member and its detector land together, which is what "closed" is for. The
corpus census run that justifies it is `corpus-work/violation-census/`
(33 hits in 8 of 10 182 documents), and the registry that remembers the accepted
ones is populated mechanically for source-side emitters by
`testkit/ts/scripts`'s `census-sync` — never for the platform's own.

## 1. Case directory layout

```
tests/pcap2test/<case-id>/
  scenario.json          # this pivot (repro variant at issue time)
  scenario.target.json   # target variant — authored WITH the fix PR, absent before
  resources/             # bodies and multipart parts, one file each
  source.txt             # human/agent-readable callflow (generated; agents read THIS, never the pcap)
  source.html            # selection/review view (generated)
```

Illustrations quote the sample documents in `testkit/scenarios/samples/`
(`simple-attempt.v3.json`, `reroute-chain.v3.json`, `noanswer-failover.v3.json`,
`retransmit-delayed-ack.v3.json`) and the fixture set in
`crates/pivot-schema/tests/fixtures/`, where `authored-cancel-race.v3.json`,
`authored-consultation-refer.v3.json` and `authored-blind-transfer.v3.json` are
the authored half of the format.

## 2. Top level

```json
{
  "pivot_version": 3,
  "case": { … },
  "identities": [ … ],
  "calls": [ … ],
  "endpoints": [ … ],
  "actors": [ … ],
  "legs": [ … ],
  "flow": [ … ],
  "deviations": [ … ],
  "rfc_violations": [ … ],
  "must_fail": [ … ],
  "postconditions": { … },
  "media": { … },
  "timing": { … }
}
```

| field | required | meaning |
|---|---|---|
| `pivot_version` | yes | `3` |
| `case` | yes | identity, provenance, replayability, required capabilities, informative annotations (§3) |
| `identities` | yes | every number and domain the document names, declared once (§8.5) |
| `calls` | yes | the calls the document plays, each with its attempt chain (§4) |
| `endpoints` | yes | sockets the lane must bind (§5) |
| `actors` | yes | simulated elements on those sockets, and their background policies (§5) |
| `legs` | yes | symbolic dialogs (§5) |
| `flow` | yes | the choreography (§6) |
| `deviations` | no | non-compliance the replay must reproduce (§11) |
| `rfc_violations` | no | RFC rules a message the flow already carries breaks (§11.1) |
| `must_fail` | no | the failures this run MUST produce. Declaring any makes the document a negative case (§11.2) |
| `postconditions` | no in schema, **yes in lint** | what must hold after settle (§10) |
| `media` | no | RESERVED, deployment-extensible (§12) |
| `timing` | yes | budgets stated once (§7) |

`deny_unknown_fields` on every object. There is deliberately no control flow in
the document: no loops, no conditionals, no `goto`. The declared alternatives of
an `alt` are the only branching, and they are declared, not computed.

Loops in particular are OUT, permanently. Authoring sugar — matrices over
topology axes, repetition — lives in the TypeScript that generates documents and
is always unrolled. The document is straight-line plus declared alternatives.

### 2.1 Canonical serialization

Serialization is canonical and formatter-owned:

- keys sorted lexically at every level;
- two-space indent;
- one trailing newline.

The Rust schema owner ships the normalising formatter (`pivot-schema fmt`,
which parses through the structs before re-emitting, so formatting a
non-conforming document fails instead of tidying it). Generator output and any
hand edit are formatted before diffing, so key order is the formatter's problem
and no emitter reproduces a struct's declaration order by hand.

### 2.2 Emptiness

Uniform, no exceptions: **every optional collection is OMITTED when empty.**
Never `[]`, never `{}`, never `null`. No field's emptiness carries meaning.

## 3. case

```json
"case": {
  "id": "…",
  "title": "…",
  "family": "reroute",
  "variant": "repro",
  "origin": "capture",
  "source": { "capture": "…", "call_groups": [0], "anonymized": true },
  "defect": { "marker": { "step": "s7" }, "description": "…" },
  "requires": ["proxy", "ha-pair"],
  "origin_lane": "captured-platform",
  "lanes": { "kind": "blocked:number-unclassified", "upstream-fake": "ok" },
  "annotations": { "flags": [ { "kind": "…", "detail": "…" } ] }
}
```

| field | required | meaning |
|---|---|---|
| `id` | yes | case directory name |
| `title` | yes | one line, human-facing |
| `family` | yes | informative layer-3 classification: `transparent`, `prack`, `mrf`, `refer`, `reroute`, `fork`. Never interpreted |
| `variant` | yes | `repro` or `target` |
| `origin` | yes | `capture` or `authored` (§3.1) |
| `source` | on a capture | capture file, upstream call groups, whether identities went through the anonymizer |
| `defect` | no | the STEP ID whose outcome IS the defect, plus a description |
| `requires` | no | capability tokens the rig must have (§3.2) |
| `origin_lane` | no | the lane whose system produced the asserted content (§9.1). Open token |
| `lanes` | yes | per-lane replayability verdict, one token per lane |
| `annotations` | no | informative sidecar. The interpreter never reads it |

`family` is computed from the flow, not from the correlation rule that formed
the case: `mrf` on a case carrying a joined media resource, `refer` on a REFER,
`prack` on a PRACK CSeq, `reroute` on a branch of length greater than 1, `fork`
on several branches, else `transparent`. The `mrf` arm is read FIRST: a joined
resource takes a branch of its own, so every arm below it would read the join as
a fork.

A `lanes` value is `ok` or `blocked:<reason>`. The generator STATES whether a
lane can replay the case, with the reason; lint checks the claim, and the driver
skips a non-replayable lane loudly instead of failing it. The lane NAMES and the
blocking REASONS are a deployment's; `pivot-schema` models `lanes` as an open
map of open reason tokens.

A document carrying a JOINED leg (§4.1) blocks every lane on
`multi-party-not-driveable`: the leg is dialled by whatever joined it, and no
lane compiles that. The reason names the LANE's missing capability rather than
the mechanism, so a REFER-joined document earns the same token and loses it the
same way.

### 3.1 origin

`origin` is the discriminator the generator-subset gate (§13) turns on, and it
is not a style flag. A captured document is a projection of packets a tool saw:
everything it carries must be readable off them, and it must carry what they
justify — `source`, per-step `observed`, `timing.capture_span_ms`. An authored
document may use the whole format and carries no capture coordinate.

### 3.2 requires

An informative list of open capability tokens (`proxy`, `mrf`, `store-faults`,
`ha-pair`). The scene a scenario runs against is NOT in this document —
hundreds of scenarios share one scene, and the runner pairs scenario × scene —
so `requires` exists to make an impossible pairing refuse loudly, through the
same skip-loudly machinery as `case.lanes`. The interpreter never reads it.

A token may imply scene PARAMETERS the document does not carry — `store-faults`
names a fault profile, `ha-pair` a node count — and that is the division of
labour, not a gap: the document states what the scenario needs, and the scene it
is paired with states how that need is met.

## 4. calls

Each entry is one call, and it owns the ONE encoding of its attempt chain.
Parallel forks are same-`position` attempts on different `branch`es; a
sequential hunt is one branch with several positions. A captured case is exactly
one call — correlation cuts a case per call, so a capture yields one DOCUMENT per
call and several files per capture, which lint enforces (§13.1) — while a
concurrency test is several calls, and their flows interleave through step ids
and `after`.

```json
"calls": [
  { "id": "c1",
    "caller_leg": "A",
    "attempts": [
      { "branch": 0, "position": 0, "leg": "B",
        "callee": { "identity": "called-0-0" },
        "final": { "status": 487, "at_ms": 15441 },
        "cause": "no-answer",
        "no_answer_ms": 15139,
        "cause_evidence": [ "SUT-originated CANCEL 15139 ms after the attempt INVITE" ] },
      { "branch": 0, "position": 1, "leg": "C",
        "callee": { "identity": "called-0-1" },
        "final": { "status": 200, "at_ms": 25404 },
        "join_evidence": "the attempts' Call-IDs are application-server derivations of one base call" }
    ],
    "relay18x": { "mode": "18X_TO_180_SDP_REMOVE", "messages": "FIRST",
                  "prack": "PRACK_MANAGED_BY_AS", "evidence": [ "…" ] } }
]
```

| field | required | meaning |
|---|---|---|
| `id` | yes | unique within the document; what a qualified position token names |
| `caller_leg` | yes | the leg that originates the call |
| `attempts` | yes | the chain, at least one entry — empty exactly when `refused` |
| `refused` | no | the routing decision dialed nobody: `{ step, evidence? }` (§4.0) |
| `abandoned` | no | the caller left before any dial crossed the vantage: `{ step, evidence? }` (§4.0) |
| `relay18x` | no | the provisional-handling profile the captured system ran, for THIS call (§4.2) |

### 4.0 refused / abandoned — the call that dialed nobody

A platform that answers the caller a failure final without dialing anybody made
a routing DECISION, and a document that stated only an empty chain would be
indistinguishable from a cut that lost the b-leg. `refused` states the decision
and names the flow step carrying the final the caller got:

```json
{ "id": "c1", "caller_leg": "A", "attempts": [],
  "refused": { "step": "s3",
               "evidence": "the caller was answered 480 and no called leg crossed this vantage" } }
```

| field | required | meaning |
|---|---|---|
| `step` | yes | the caller-leg step carrying the 3xx-6xx final the refusal answered with |
| `evidence` | no | why the cut reads this vantage as a refusal rather than as a dial it lost |

The status, the reason phrase and the headers are NOT restated here — the step
already carries them, so the two cannot disagree. Lint refuses a call that
states both a refusal and a chain (`call/refused-with-attempts`), one whose
`step` is no step (`ref/refused-step-unknown`) and one whose step is not a
caller-leg final (`call/refused-step-not-a-final`); a call that states neither a
chain nor a refusal stays `call/no-attempts`.

A refused call arms no ring, runs under no provisional profile and reaches no
`/calls/failure`: the refusal is its whole lowered program (§4.3), and the lane
names no egress for a leg that never happens.

A third outcome sits beside those two: the CALLER abandoned the call. It
cancelled its own INVITE and no called leg ever crossed the vantage, so the
platform dialed nobody without deciding anything, and the `487` that follows is
the final RFC 3261 §9.2 owes a cancelled INVITE rather than a decision.
`abandoned` states it and names the caller-leg step carrying that CANCEL:

```json
{ "id": "c1", "caller_leg": "A", "attempts": [],
  "abandoned": { "step": "s3",
                 "evidence": "the caller cancelled its own INVITE and no called leg crossed this vantage" } }
```

Lint refuses an abandon that still dials (`call/abandoned-with-attempts`), one
stated beside a refusal (`call/abandoned-with-refusal`), one whose `step` is no
step (`ref/abandoned-step-unknown`) and one whose step is not a CANCEL the
caller leg SENT (`call/abandoned-step-not-a-cancel`). An abandoned call states
no decision at all, so a lane routes it and points the dial at nothing.

### 4.1 attempts

| field | required | meaning |
|---|---|---|
| `branch` | yes | parallel branch index |
| `position` | yes | position within the branch's sequential chain. EXPLICIT, not array order |
| `leg` | yes | the leg id the flow uses for this attempt |
| `callee.identity` | yes | NAME of an `identities` entry (§8.5). The number is never embedded here |
| `final` | no | the attempt's own terminal INVITE final at its vantage: `{ status, at_ms }` |
| `cause` | no | why the platform left this attempt. Closed vocabulary, below |
| `joined_by` | no | what ADDED this leg to a running call: `{ kind: "refer" \| "mrf", step }`. Independent of `cause` |
| `cause_evidence` | no | the captured signals that justify `cause` |
| `join_evidence` | no | why the correlator put THIS attempt in that chain |
| `no_answer_ms` | no | INVITE-to-SUT-CANCEL dwell, present exactly when `cause` is `no-answer` |

`position` is explicit so the list stays reorderable under the sorted-key
formatter and so `(branch, position)` is a stable key.

Evidence splits deliberately. `cause_evidence` explains a failure and belongs to
the attempt that failed; `join_evidence` explains a correlation and belongs to
the attempt that joined.

`cause` vocabulary, every member read off a captured datagram: `no-answer`,
`busy`, `transaction-timeout`, `closed:bye`, `redirect:<3xx>`,
`external:<4xx-6xx>`.

**A `cause` may cite only two things: the attempt's own DIALOG-CREATING final,
or an actual closer — a BYE or a CANCEL.** An in-dialog final answers a
renegotiation of a dialog that already exists, and answering a renegotiation
with a negative status does not close anything: the dialog stands, and what
then releases the leg is the BYE somebody sends. So an in-dialog final is
`cause_evidence` and never a cause, and `closed:bye` is how an answered attempt
the platform released states its exit. Once a message is in-dialog the
direction it travelled is irrelevant to this rule; what matters is that no
in-dialog final ever ended a leg.

The rule is checkable because the flow says which finals are in-dialog
(§6.1): lint refuses a `cause` citing a status the leg only ever saw in-dialog
(`cause/in-dialog-final`), and a `closed:bye` on a leg whose flow carries no
BYE (`cause/closer-missing`).

What made the platform send that BYE — a renegotiation the far end refused, a
transfer-failure decision — is evidence, not vocabulary. The closer is the
datagram; the reason is the reading, and a reading belongs beside the fact.

**A call is one calling leg plus n called legs.** Every `cause` member but
`closed:bye` is a FAILURE, so a chain states a sequential hunt. `closed:bye` is
the one exit that is not one: the attempt ANSWERED and the platform later
released it, and an attempt after it states a reroute of a released leg, not a
further hunt. A transferee leg
is not a failure — the first attempt ANSWERED, and the call then dialed somebody
else because a REFER said so — and a media resource inserted mid-call is not one
either. Those legs JOIN the call: they take their own `branch` and state
`joined_by`, whose `step` is the flow step that performed the join (the REFER
the platform accepted, the request that inserted the resource).

**`joined_by` and `cause` are orthogonal.** One says how a leg ENTERED the call,
the other why the platform LEFT it, and a leg that joined fails like any other.
Two corners the format must state, and does: a reroute after a join, where the
joined leg is dialed, fails and the hunt goes on; and an automatic transfer
whose target is busy, where the platform reroutes INTERNALLY and never notifies
the transferor — one attempt carrying `joined_by` AND `cause: busy`, followed by
the onward attempt at the next position of the same branch.

`joined_by.step` must name a step on a leg of the SAME call:
a join is an event inside one call. It must also be a step that runs
UNCONDITIONALLY — not one inside an `alt` branch, and the alt's own id is no
substitute, since a join names the message that performed it — and it must
precede the joined leg's own first message, because it is what added the leg.

`calls[]` plurality stays reserved for what really is more than one call — a
call-limiter test, a consultation placed beside the call it will transfer.

`no_answer_ms` is declared only where the attempt RANG and nothing else explains
the give-up. A caller that answered or hung up mid-attempt makes the SUT's
CANCEL a propagation rather than a timer, and the cause is refused. The value is
refused outside 1 000 to 3 600 000 ms: below that a whole-second timer arms
nothing, above it a "ring" is a mis-cut span. The refusals are as normative as
the value.

### 4.2 relay18x

The provisional-handling profile the CAPTURED SUT ran, in the Routing API's own
vocabulary so a lane applies it without re-deciding.

| field | required | meaning |
|---|---|---|
| `mode` | yes | open profile token (e.g. `18X_TO_180_SDP_REMOVE`, `18X_TO_180_SDP_TRANSPARENT`) |
| `messages` | yes | open token: how many upstream `18x` reach the caller (this deployment emits `FIRST`, `ONE_PER_VALUE`, `ALL`) |
| `prack` | no | open prack-mode token (`PRACK_MANAGED_BY_AS`), present only where the SUT answered 100rel itself |
| `evidence` | yes | the detection signals that fired |

Every token here names a platform's configuration, not a SIP constant. The
structs model all three as open strings and lint checks none of them against a
list; the values above are what this deployment's detector emits, and a
different platform's vocabulary is as valid.

### 4.3 Per-lane compilation contract

**The driver compiles `calls`. The interpreter never interprets it.**

`calls` states intent. Before a run, the TS driver lowers each call into
whatever the target lane needs:

- every lane: a binding from each `identities` name to a real number. The
  TRANSLATION is the lane's — a mock's allocation and a provisioned backend's
  leased number are different numbers for one entry — and it is what makes
  `${num:…}` resolvable;
- mock lanes: injected directive headers on the outbound INVITE (attempt order,
  causes, dwells, provisional profile);
- a real backend: provisioning choices, leased conformance numbers, route
  configuration;
- fake lane: number allocation per attempt and the armed no-answer timers.

The interpreter receives a compiled run configuration and the flow. It sequences
the legs the flow names. It never reads `cause`, `cause_evidence`,
`join_evidence` or `relay18x`.

A lane numbers by attempt INDEX, so every branch's attempt `s` is dialed one
number, and two such claims sharing an endpoint stay ambiguous — which lint
states as `claim/same-number-ambiguous`. The two-timer contract (a per-call
timer stated per attempt, always strictly below the global bound) is unchanged.

**A lowered route plan states branch 0's chain and nothing else.** A joined
branch is dialled by whatever joined it — the mechanism `joined_by` names, a
REFER or an MRF join (§4.1) — so a plan that also stated its chain would be
instructing a dial the run does not make.

**A lowered directive is stated PER CALL.** A document declaring two calls dials
two callees, so what the lane states about egress — the destination, the route
plan, the admission entry the call is counted against — is keyed by `calls[].id`
and reaches that call's OWN dial: the INVITE that opens its caller leg, and no
other message. One directive serving a whole run would deliver the second call to
the first one's endpoint. What the lane states about the RUN — a test-correlation
header, a lane artifact every message carries — stays run-level.

### 4.5 Tier-2 position tokens

The tier-2 reference namespace (§8) resolves against these chains:

```
caller | called[<branch>][<position>]                  # bare
<call-id>.caller | <call-id>.called[<branch>][<position>]   # qualified
```

The bare form names the document's single call. A document declaring more than
one call must qualify every position, since a bare token would name one of two
chains.

## 5. endpoints, actors, legs

```json
"endpoints": [
  { "id": "ep0", "observed": "192.0.2.153:5081", "side": "peer", "binding": "loopback" }
],
"actors": [
  { "id": "uac1", "kind": "uac", "endpoint": "ep0", "identity": "caller" },
  { "id": "uas1", "kind": "uas", "endpoint": "ep0", "claim": { "by": "ruri-pos" },
    "background": [ { "match": { "method": "OPTIONS" },
                      "respond": { "status": 200 },
                      "count": { "at_least": 1 } } ] }
],
"legs": [
  { "id": "A", "actor": "uac1", "dir": "out", "media": { "rtp": "book" } }
]
```

An **endpoint** is one mux socket. An **actor** is one simulated network element
on an endpoint. A **leg** is one symbolic dialog.

| endpoint field | required | meaning |
|---|---|---|
| `id` | yes | reference id |
| `observed` | yes | the captured socket. A lane binding by address uses it |
| `side` | yes | what the endpoint IS relative to the SUT: `peer` or `sut` |
| `binding` | yes | how the lane must bind it: `dedicated` or `loopback` |

`binding` is stated PER ENDPOINT: a case may mix a loopback vantage on attempt 1
with a dedicated one on attempt 2. The generator computes it as "this endpoint
hosts both a UAC and a UAS".

| actor field | required | meaning |
|---|---|---|
| `id` | yes | reference id |
| `kind` | yes | `uac`, `uas`, `mrf` |
| `endpoint` | yes | endpoint id |
| `identity` | no | NAME of an `identities` entry (§8.5) — the CALLER's, in practice. A callee's is named by its attempt |
| `claim` | no | how a UAS claims its inbound INVITE: `{ "by": "ruri-pos" \| "arrival-order" }` |
| `background` | no | traffic answered outside the flow (§5.1) |

`claim` carries `by` and nothing else: which attempt an actor plays is already
stated by `attempts[].leg`.

`legs`: `id`, `actor`, `dir` (`out` = the actor originates), optional `media`.
Call-ID, tags, CSeq base and route set are runner state, referenced only through
the leg id — and through the leg accessors of §8.

### 5.1 background

```json
"background": [ { "match": { "method": "OPTIONS" },
                  "respond": { "status": 200 },
                  "count": { "at_least": 1 } } ]
```

The full shape is AUTHORED ONLY — with ONE generated exception. A generated
document states exactly `{ "match": { "method": "OPTIONS" }, "respond":
{ "status": 200 } }` (no count) on every actor the deployment's policy says
answers the replaying SUT's in-dialog OPTIONS audit; lint's subset gate refuses
any other generated shape. The same policy claims the CAPTURED platform's own
locally-minted exchanges of a policy-answered method OUT of the flow (its
cadence is a system parameter, not call behaviour), which is the stated
exception to §6's every-message rule; a relayed end-to-end exchange, an
out-of-dialog probe, and a request the endpoint emits all keep their steps.

A message matching a policy **never touches the flow cursor**. It is answered per
`respond`, recorded in the run bundle, and never satisfies an `expect`. No flow
step is ever written for one.

A policy answers traffic OUTSIDE the flow, so it yields to the flow: an arrival
a frontier `expect` on that leg is OPEN on — its budget started (§6.8) and its
discriminator satisfied — is that step's, never the policy's. Both halves are
load-bearing where the system RELAYS a policy-answered method: the relayed
request is call behaviour a step owns, while the same method arriving before
that step's window opens is the replaying SUT's own audit, whose cadence the
flow must never gate on. Nothing on the wire separates the two; the window does.

`count` is the assertion, and it is checked **at settle**, never mid-flow:
"at least one OPTIONS reached this endpoint" is a fact about a period, not about
a position in a sequence. `at_least`, `at_most` and `exactly` are the bounds;
`exactly` does not combine with the other two. Stating no `count` answers the
traffic and asserts nothing about it. The period ends with the run's settle
budget, settled or not: a run that failed to settle still reports what its
endpoints heard.

**A count of ABSENCE is evidence only where the lane's egress could have reached
the party it counts.** `exactly: 0` on an endpoint the run can never dial is a
check that cannot fail. It counts because the lane states each call's own egress
(§4.3): the run CAN reach that callee, and the counter says nothing did.

`match` is method-only. A policy that had to inspect a header would be a flow
step wearing a disguise.

## 6. flow

An ordered list of NODES. Four kinds, discriminated by `op`:

| `op` | node | §  |
|---|---|---|
| `send`, `expect` | one message at one actor's vantage | 6.1 |
| `inject` | an external event handed to the lane's injector | 6.6 |
| `alt` | declared alternatives; exactly one branch runs | 6.5 |
| `unordered` | messages that must all arrive, in any order | 6.5 |

Every node carries an `id` and may carry `after`. Block ids and step ids share
one namespace: a reference names either.

**A captured flow lists EVERY captured message**, protocol-mechanical automatics
included, so a reader can tell "elided automatic" from "never happened". The one
exception is an exchange a `background` policy claims (§5.1): the policy is the
document's record of it, flagged `background-claimed` by the generator.

Nesting stops at one level: an `alt` branch and an `unordered` group hold
message steps, never further blocks. The interpreter commits to a branch on its
FIRST message, and a nested block has no first message to commit on.

### 6.1 A message step

```json
{ "id": "s2", "leg": "A", "op": "expect",
  "auto": true,
  "check": "record",
  "msg": { "status": 100, "reason": "Trying", "cseq-method": "INVITE", "cseq": 1 },
  "delay": { "ms": 2, "from": "step:s1", "compressible": true, "timer_linked": false },
  "observed": { "leg": 1, "msg": 1, "at_us": 2532 } }
```

| field | required | meaning |
|---|---|---|
| `id` | yes | unique within the document; what every reference names (§6.2) |
| `leg` | yes | leg id |
| `op` | yes | `send` or `expect` |
| `auto` | no | `true` when the interpreter's own stack owns this message (§6.3) |
| `in_dialog` | TOTAL | `true` on every step after the leg's dialog-creating final, absent at or before it (§6.1) |
| `confirms_dialog` | on that one ACK | `true` on the ACK answering a dialog-creating final, absent on every other step (§6.1) |
| `retransmits` | no | captured retransmissions of THIS message, beyond the first (§6.9). On a `send`, a paced or drawn class only |
| `check` | `expect` only | `assert` or `record` (§6.4) |
| `optional` | `expect` only | tolerated absence — AUTHORED ONLY (§6.5) |
| `after` | no | ids of nodes that must have completed first — AUTHORED ONLY (§6.7) |
| `checks` | no | field assertions over the matched message — AUTHORED ONLY (§9) |
| `early` | no | id of the early dialog this message answers under, or rides |
| `overlap` | no | a declared race with the named step |
| `msg` | yes | message spec (§8) |
| `delay` | yes | dwell and its anchor (§6.8) |
| `within_ms` | no | per-step override of `timing.expect_budget_ms` |
| `observed` | on a capture | capture coordinate (§6.10) |

There is no `expect-claim` op. A UAS's first inbound INVITE is a claim by virtue
of its actor's `claim`.

`in_dialog` is generic and method-blind: it says where the transaction sits,
not what it is. The method NEVER substitutes for the marker — an OPTIONS rides a
dialog or does not, and only the marker says which — so the marking is TOTAL and
positional, and a document states it everywhere it holds rather than where a
reader would miss it.

**A leg's DIALOG-CREATING FINAL is the first `2xx` response to an INVITE on that
leg. Every step of that leg strictly AFTER it carries `in_dialog: true`; every
step at or before it carries no marker.** Strictly after, because the dialog
exists the moment that final's To-tag arrives (RFC 3261 §13.2.2.4), so the ACK
that answers it is already a request within the dialog (§13.2.2.4 sends it
"constructed as described in §12.2.1"), not a piece of the INVITE transaction —
§17.1.1.3's transaction-owned ACK is the one to a NON-2xx final, and that ACK
runs on a leg with no dialog-creating final at all. So the ACK to the
dialog-creating 2xx is marked, and so is every BYE, PRACK, UPDATE, re-INVITE,
INFO, OPTIONS, NOTIFY and REFER after it, and every response to those. A leg
whose INVITE never took a 2xx carries no marker anywhere.

Two exclusions, both because the message belongs to a transaction that is not
the dialog's. A CANCEL and any response whose `cseq-method` is CANCEL, wherever
they sit: a CANCEL is scoped to the INVITE transaction it cancels (RFC 3261
§9.1) and is never sent within a dialog (§12.2), and a 200 to a CANCEL that
crossed a 200 to the INVITE is a race, not a renegotiation. And a step inside an
`alt` branch reads only the dialog-creating finals its OWN run reaches — the
unconditional prefix plus its own branch — so a 487 in the cancelled branch of a
cancel race is unmarked even though the answered branch's 200 sits earlier in
the document.

`in_dialog` and `early` are ORTHOGONAL, never alternatives, and the boundary
above is what keeps them from overlapping: `in_dialog` means CONFIRMED, so every
early-dialog message — a reliable provisional, the PRACK that rides it, an
UPDATE before the final — sits at or before the dialog-creating final and takes
no marker, and `early` alone says which fork it rides. The two co-occur where a
message after the leg's first dialog-creating final belongs to a FURTHER dialog
on the leg: a later fork's own 2xx to the forked INVITE — a second dialog, which
the UAC ACKs like the first (RFC 3261 §13.2.2.4) — and the ACK the leg expects
for it, where `early` gates the To-tag and `in_dialog` states that a dialog is
up. The cut leaves the ACK to the fork that answered first unnamed — it rides
the dialog `in_dialog` states — and lint pairs it by the 2xx it discharges; an
ACK that does name its fork pairs by the tag either way. An ACK the leg sends
names none, since a request send names a fork only where it rides one, and no
request send inside a confirmed dialog does.

What consumes the marker is §4.1's citation rule, and `pivot-schema lint` is
what enforces the totality (`in-dialog/missing`, `in-dialog/outside-dialog`).
The interpreter gates nothing on it.

**The ACK that answers a dialog-creating final carries `confirms_dialog: true`,
and no other step does.** It completes the handshake that final opened
(RFC 3261 §13.2.2.4). Which ACK that is says something about the DIALOG, not
about the message: an ACK to a re-INVITE's 2xx is the same method on the same
leg inside the same dialog, and the two are told apart by nothing the message
carries. A method never substitutes for a marker and neither does a CSeq chase,
so the document states which ACK it is.

The marker rides BESIDE `in_dialog`, never instead of it: that ACK sits strictly
after the final, so it carries both. It rides beside `early` too, where the ACK
names the fork it answers. Under forking each answered fork mints its own dialog
on the one leg and each is confirmed by its own ACK, so the marker is stated once
per DIALOG rather than once per leg: a leg answered 2xx under two To-tags carries
it twice. An ACK that names its fork pairs with the final by the tag; one naming
none pairs with the fork's 2xx it discharges.

Two ACKs never carry it. An ACK to a re-INVITE's 2xx, which renegotiates a
dialog that is already up. And an ACK to a NON-2xx final, which is §17.1.1.3's
transaction-owned ACK, on a leg that may hold no dialog at all. Which ACK
answers the final is read by LEG STATE — each ACK discharging the newest INVITE
transaction in its direction that holds a final and no ACK yet — and never as
the first ACK the leg carries after the final: a re-INVITE sent over the
un-ACKed 2xx (§14.1) is answered 491, its ACK runs first and discharges the
re-INVITE, and the 2xx's own ACK behind it is the one that confirms. The
reading is by position, with no CSeq consulted, so a leg whose ACKs run in the
other order (the 2xx's ACK before the 491's) is read the other way round. The
cut stamps by that reading and lint checks it by the same one; under forking an
ACK naming its fork pairs by the tag instead.

The marking is REQUIRED the way `in_dialog` is: a confirmed dialog whose
answering ACK states no marker is an error. Requiredness is about the MARKER and
not about the message — a flow carrying no ACK after its dialog-creating final
declares no confirming ACK, and §11.1's `no-ack-to-dialog-creating-2xx` is what
speaks to an ACK that never came. `pivot-schema lint` enforces the placement
(`in-dialog/confirm-missing`, `in-dialog/confirm-not-ack`,
`in-dialog/confirm-outside-dialog`, `in-dialog/confirm-not-the-answer`,
`in-dialog/confirm-duplicate`). The interpreter reads nothing from the marker at
run time, exactly as it reads nothing from `in_dialog`: lint is the enforcement.

**An `early` id names one fork, and two kinds of message may carry it.** A
response `send` ANSWERS under it, minting the To-tag that fork rings on
(RFC 3261 §12.1.1). A PRACK (RFC 3262 §7.2) or an UPDATE (RFC 3311 §5.1) RIDES
it: both run inside a dialog that has not confirmed yet, and the id says which
fork. Every other request waits for a confirmed dialog, so `early` on one says
nothing and is refused. An UPDATE naming no fork on a leg whose dialog is
unconfirmed rides that leg's ONE early dialog; a leg ringing several and an
UPDATE naming none is a refusal, because which fork an UPDATE rides decides
which endpoint receives it and §14 has the tool infer nothing.
`${early:<id>.tag}` and `${early:<id>.rseq}` (§8.1) read the fork back.

On an EXPECT, `early` GATES: the message satisfies the step only if its To-tag
is the one the named fork rings on. A leg ringing two forks takes provisionals
whose discriminators are identical, and the tag is what tells them apart
(RFC 3261 §12.1.1).

### 6.2 Step identity

Every step carries a unique string `id`: tool-generated for a capture (`s1`,
`s2`, … dense and 1-based over the flow), author-chosen otherwise. **Every
reference in the document is id-based** — `delay.from: "step:<id>"`, `after`,
`deviations[].step`, `case.defect.marker.step`, the `${step:…}` accessor. Numeric
step ordering died with v2, and with it the class of bug where inserting a step
silently moved a marker.

An id carries no `.`: the accessor grammar splits an id from its field at the
first dot.

### 6.3 auto steps

`auto: true` marks a message the interpreter's stack COMPOSES: 100 Trying,
ACK-to-final, PRACK and its 2xx. What the marker says is where the message's
coordinates come from — **the stack derives its R-URI, Route set, Via and CSeq
from the TRANSACTION that obliged it, not from dialog state**:

| step | R-URI | Route | Via / branch | CSeq | extra |
|---|---|---|---|---|---|
| scripted in-dialog request | `dialog.remote_target` | `dialog.route_set` | fresh | `local_cseq++` | — |
| ACK to a 2xx | `dialog.remote_target` | `dialog.route_set` | fresh | **the outstanding INVITE's** | — |
| ACK to a non-2xx | **the INVITE's R-URI** | **the INVITE's Route, verbatim** | **the INVITE's, same branch** | **the INVITE's** | From/To/Call-ID echoed off the response |
| PRACK | `dialog.remote_target` | `dialog.route_set` | fresh | fresh | **`RAck` off the provisional received** |
| 100 Trying | — | — | echoed off the request | echoed off the request | — |

**It says nothing about STORAGE. An auto step stores what any step stores** —
the three tiers of §8, unchanged: tier-1 omitted, tier-2 as positional refs,
everything else frozen in wire order, and the body where the message carried
one. There is no second storage model, and a step's `msg` never depends on who
composes the message.

The ONE thing the marker withholds is a body the stack has nowhere to place.
Three classes carry one: the **ACK to a 2xx**, which is where a delayed offer's
ANSWER rides (RFC 3261 §13.2.1), and **PRACK with its 2xx** (RFC 3262 §5). Two
do not, and a body stored on them would be emitted by nobody, so lint refuses it
(`auto/body-not-composable`): a **100 Trying**, which negotiates nothing, and
the **ACK to a non-2xx**, absorbed by the INVITE transaction (RFC 3261
§17.1.1.3) and reaching no TU that could read one. An EXPECT composes
nothing — its body, a shape or a resource, asserts what arrived (§8.3) — so an
expect may state one on any class, and the relayed ACK's SDP resource is what
lets the confrontation see a dropped answer at all.

`cseq` is the CSeq NUMBER the CAPTURE carried, and it is a PAIRING TOKEN: it
names which transaction the captured automatic belonged to, for confrontation,
for lint, and as the marker that scopes an auto ACK's drawn `retransmits` to one
transaction. It is never REPLAYED and never RESOLVED AGAINST — the interpreter
numbers its own CSeqs, so a captured number is the authoring platform's and
names nothing in the run. An auto ACK finds the final it acknowledges in LEG
STATE: the newest final still answering an INVITE this leg sent and has not yet
ACKed, and where every one is discharged, the newest answering any of them. RFC
3261 §14.1 leaves one INVITE outstanding per dialog, so a compliant peer offers
one candidate. A captured peer that pipelined a second re-INVITE before ACKing
the first's 2xx offers two, and each ACK discharges its OWN, so the deferred 2xx
still stands for the step that owes it. A captured peer that ACKed one 2xx TWICE
leaves none outstanding, and the fallback is what keeps the second step
composable. `cseq` never appears on a scripted step.

An auto EXPECT always carries `check: "record"` — the stack owns the message, so
there is nothing for the document to assert. An auto SEND carries no `check` at
all, for the same reason every send does not: a send is emitted, not checked.

An auto step's measured `delay` is how a held automatic is expressed. A WITHHELD
automatic is not a deleted step: it is the step plus a `suppress-auto` deviation
pointing at it (§11).

**`retransmits` on an auto ACK step means one ACK per repeat of the final its
own transaction drew**, not a paced ladder: an ACK rides no retransmission
timer, and the UAC core owes one ACK per 2xx it RECEIVES (RFC 3261 §13.2.2.4),
so the count is drawn by the wire and measured against the repeats of that one
final. Which copies draw one turns on WHOSE ACK answers the final, and the two
paragraphs below state that: the ACK to a 2xx is the acknowledging peer's own,
the ACK to a non-2xx final the client transaction's. `cseq`
is the MARKER that a step
carries one transaction to draw against — a count on an ACK that is not an
automatic, or on one stating no `cseq`, is refused.

**On an `expect` the generator DERIVES that count from the final's own ladder**,
never from the ACKs the capture held: the peer answering it is the platform
under replay, not the one the packets came off, so a captured platform that
under-ACKed a repeated final states its own non-compliance and nothing about
what the run will see. The `send` half is the scripted actor's behaviour and
stays exactly as captured — a caller that answers three copies with one ACK is
modelling a peer, which is the document's to state. `ack-count-drawn-from-final`
names every expectation whose count the capture did not already hold.

**The send half states a peer's non-compliance, ACKs it cannot draw included.**
The UAC core owes one ACK per 2xx it RECEIVES (§13.2.2.4), so a captured caller
that answered ONE 2xx with two ACKs on distinct branches sent one datagram no
final drew. That second ACK is a `send` step like any other and the run emits
it: a send models the peer, and the peer's breach is the shape the corpus
recorded. It is a fresh transaction on the dialog, not a `repeat_of` — the
branches differ — so the ACK ladder above never sees it, and the interpreter's
own discharge bookkeeping must not deny it its final.

*Amended 2026-08-27 (issue 103), narrowed 2026-08-28 (issues 149, 150).* The count is
the final's own ladder, and the ladder is the only thing that puts an ACK on the
leg: a B2BUA owes one ACK per final it RECEIVES here, so N ACKs arriving from
the far leg draw none of their own.

**Relayed from the far leg.** The ACK to a 2xx is the ACKNOWLEDGING PEER's own,
relayed: §13.2.2.4 gives the UAC core ONE ACK per 2xx and re-passes THAT ACK to
the transport for every copy, so on a relayed INVITE the ACK this leg owes goes
out when the far leg's arrives — initial INVITE or re-INVITE, offer in the
INVITE or delayed offer (§13.2.1), body or none. A copy inside the wait is
answered by the single ACK that follows it and draws none of its own; a copy
landing once the ACK exists draws a re-send of that datagram, body included.
The wait is read off the two `observed` coordinates, and the rungs off the
final's measured `retransmit_intervals_ms` where it states them, §6.9's class
ladder where it does not.

**Displaced by the next INVITE.** The leg holds exactly ONE such ACK — the
datagram the relay retained — and a new INVITE transaction on the leg resets it,
so a copy of a superseded final draws nothing and the count drops by one per
copy landing past that reset. Which copies those are is read off the document's
own DELAY GRAPH, each step's `delay.from` walked back to the anchor it names,
not off the `observed` coordinates: when the SUT opens that later transaction is
what the document's own delays say, not an instant the source platform measured.
Only the band between the ACK and the reset draws.

**Composed on arrival.** An ACK to a NON-2xx final is the client transaction's
own (§17.1.1.3): composed from the final itself, hop by hop on every platform,
so every copy of that final draws one back. The FINAL's status decides which of
the two paragraphs applies — never the body the ACK carries, and never whether
it CONFIRMS the dialog. Both shapes are pinned in
`crates/b2bua-harness/tests/repeated_2xx_before_caller_ack.rs` and
`crates/b2bua-harness/tests/it/ack_body_relayed.rs`.

An auto PRACK acknowledges the reliable provisionals outstanding on its leg
OLDEST FIRST: N provisionals are acknowledged by N PRACKs, in arrival order.
RFC 3262 §3 numbers the provisionals of one transaction consecutively, so
oldest-first is what keeps a PRACK's `RAck` naming the provisional it answers.

### 6.4 check: assert or record

`check` is on `expect` steps only, and it is stated per step. A `send` is
emitted, not checked, so the field is absent there rather than present and
meaningless.

| value | contract |
|---|---|
| `assert` | the stored content of `msg` is MATCHED against the inbound message |
| `record` | the stored content is RECORDED for post-run confrontation and matched against nothing |

Under both values the step still gates on `op`, leg alignment, and the
discriminator (`method`, or `status` + `cseq-method`), and still times out at
`within_ms`.

**A lane never reinterprets a stored field.** The rule the generator applies: a
message some captured peer emitted and the SUT relayed is asserted; a message the
SUT MINTED is recorded.

Where an asserted message carries a header only the origin platform emits, the
scoping is per HEADER and lives in §9.1 — a class on that header, not a weaker
`check` on the whole message.

### 6.5 Acceptable nondeterminism — AUTHORED ONLY

Three constructs, and they answer three different questions.

**`alt`** — which of several things happened.

```json
{ "id": "a1", "op": "alt", "branches": [
  { "name": "answered",  "steps": [ { "id": "s11", "leg": "A", "op": "expect", … } ] },
  { "name": "cancelled", "steps": [ { "id": "s15", "leg": "A", "op": "expect", … } ] }
] }
```

The interpreter commits on the first discriminating message and **never
backtracks**. Lint enforces discriminability structurally: at least two
branches, no empty branch, no branch opening on a `send` (which branch runs is
decided by what ARRIVES), no branch opening on an `optional` step, and no two
branches sharing a first discriminator — leg plus method, or leg plus status
plus `cseq-method`. A later assertion may cite which branch ran, through
`${step:<alt-id>.branch}`.

**`optional: true`** on an expect — tolerated absence. The step is released when
a later step on the same leg MATCHES first, and released again — rather than
failed — when its OWN budget (`within_ms`, else `timing.expect_budget_ms`)
expires.

Both releases are normative, and the budget one is what keeps a tolerated
absence from wedging the steps behind it forever: nothing else states when the
run may stop waiting for a message the document already said may never come. It
can only ever release a step the document declared absent-tolerant, so it
weakens no assertion. A step that MATCHED is not released; it completed.

A `send` never releases an `optional` expect it overtakes. §6.5 releases on a
MATCH, and a send is emitted, not matched — releasing on one would discard a
message still in flight, which then arrives as an unmatched datagram.

**`unordered`** — all of these must arrive, order free.

```json
{ "id": "u1", "op": "unordered", "steps": [ … ] }
```

At least two steps, and every one an `expect`: the runner controls when it
sends, so an order-free group holds only what it WAITS for.

#### Referencing a step inside a block

A step inside an `alt` branch exists only on the run that chose that branch, so:

- **from inside the same branch**, a later step may reference it normally;
- **from anywhere else** — another branch, a step after the alt, a
  postcondition — the reference is to the **alt node's own id**, which means
  "the alt completed, whichever branch ran". Referencing a branch step from
  outside is refused (`order/cross-branch`, `accessor/cross-branch`).

This is what keeps the interpreter free of a case it would otherwise have to
invent semantics for: a delay anchored on, or an `after` waiting for, a message
that never arrived.

An `unordered` group is the opposite case and is treated as such: every member
arrives on every run, so a later step may reference any of them by id. What is
refused there is a reference BETWEEN two members, since an order-free group has
no internal order to be earlier in.

### 6.6 inject — AUTHORED ONLY, SHAPE ONLY

```json
{ "id": "i1", "op": "inject", "action": "store-fault:LiveAudit",
  "target": "consultation-dialog", "after": ["s10"],
  "delay": { "ms": 100, "from": "step:s10", "compressible": true, "timer_linked": false } }
```

An external event, anchored by step refs like everything else. `action` is an
open token from a deployment-owned injector registry (`store-fault:LiveAudit`,
`http:bl-cut`, `node-kill`); `target` is in the injector's own vocabulary.

**The interpreter never executes an action.** It calls an injector interface the
lane provides. That one mechanism spans store faults, HTTP-fabric faults and HA
node kills. Setup-time knobs do NOT ride here — those are the scene (§3.2).

v3 freezes the shape; execution semantics are not defined in this program.

### 6.7 after — AUTHORED ONLY

```json
"after": ["s10", "i1"]
```

The one cross-leg and cross-call ordering device: the interpreter waits until
every referenced node has completed. Same-leg ordering remains list order.

Ordering is **message-mediated only**. There are no state predicates: "after leg
B confirmed" is written as a reference to the ACK step. A reference must point
backwards; a forward `after` is a deadlock, and lint refuses it. A reference
into an `alt` branch obeys §6.5's branch-scoping rule.

A captured document carries no `after`: the chain barrier is derivable from
`attempts[].leg` plus `position`, and a capture cannot justify more.

### 6.7a overlap — the declared race

```json
"overlap": "s13"
```

Same-leg order is list order and it BINDS (§6.7b is its one exception), so a
document that lists two steps in the order the capture happened to see them has
decided that order. `overlap`
is how it declines to: the two steps arm TOGETHER on their leg's frontier, and
whichever the wire settles first settles first. The relation is symmetric —
either side may carry the field — and it holds only between NEIGHBOURS on ONE
leg, which is what an interpreter can arm together; across legs there is no
order to revoke, and over a gap the skipped steps' order is real. A plan
refuses both.

A capture DOES generate one, and this is the shape that obliges it: a leg
carrying a `send` and the relay of a cross-leg origination, both anchored on the
same step. A relay is `propagated` — anchored at a synthetic ~0 precisely
because the hop's latency belongs to whichever system replays the capture
(§6.8) — while the send's dwell is the document's own. The captured order is
then the difference between a dwell the document holds and a latency it does
not, and any SUT quicker or slower than the captured platform by that difference
inverts it. Two BYEs crossing 1.8 ms apart on one leg is the canonical case: the
capture states which arrived first, and nothing a lane can reproduce makes it so.

Only a shared anchor declares a race. Two dwells measured from two anchors are
both the document's, however close together they sit, and their order stands.

### 6.7b The undeclared race: a relay behind a send

`overlap` needs two steps a document can name as a pair, adjacent on one leg.
One relay racing a WINDOW of steps is not that shape, and no pairwise field
reaches it. The interpreter states it instead, from what the delay already says.

An `expect` measured at a SYNTHETIC ZERO from a completed step on ANOTHER leg is
a relay, and the zero is the whole tell: the hop's latency belongs to whichever
system replays the capture, so the document states none of it (§6.8). Its cause
has already fired, so the message may already be on the wire. Ordering it behind
a `send` this leg has not made yet orders an arrival behind a decision that
cannot cause it — and any system quicker than the captured one by that latency
delivers it to a leg waiting for something else.

So such a step is ARMED BESIDE the send it stands behind. Four bounds, and no
fifth:

- the anchor must have COMPLETED — before that nothing has provoked the message;
- the dwell must be the synthetic `ms: 0` and not `timer_linked`. A cross-leg
  anchor carrying a real dwell is a duration the document DOES hold, and §6.7a's
  closing rule stands for it;
- only a SEND is walked past. Once the leg is waiting on an arrival of its own,
  which of the two comes first is what list order says, and it binds;
- the leg must be able to TELL IT APART: an `expect` already armed here with the
  same discriminator would let the earlier step's datagram settle the later one.

A message step only: an `alt` armed early could COMMIT before the send in front
of it goes out, and an `unordered` group arms its whole membership. Neither is a
relay standing in a queue, and a capture carries neither (§13.1).

This is the ONE place where same-leg list order does not bind, and it un-asserts
exactly what the capture never established: the order between a dwell the
document holds and a latency it does not.

### 6.8 delay and dwell

```json
"delay": { "ms": 700, "from": "step:s14", "compressible": false, "timer_linked": true }
```

| field | required | meaning |
|---|---|---|
| `ms` | yes | milliseconds from the anchor |
| `from` | yes | the anchor: `"trigger"` or `"step:<id>"` |
| `compressible` | yes | whether a virtual-clock lane may compress this dwell |
| `timer_linked` | yes | whether the dwell interacts with a SUT or session timer |

Every message step carries a `delay`, auto steps included; an `inject` may carry
one. All delays are relative to an explicit anchor; there are no absolute times
in a pivot, and an anchor always points backwards.

`compressible` and `timer_linked` are STATED, not derived. A lane that
compresses reads `compressible` and nothing else. A timer-linked dwell is never
compressible — compressing what a system timer measures changes what the test
proves — and lint refuses the combination. What a run accepts around a
timer-linked dwell it OBSERVES is the run's own window (§9.2).

**The dwell is what the two ops share.** On a `send` it is SLEPT: the message
goes out `ms` after the anchor completed. On an `expect` nothing is slept — the
message arrives when it arrives — but the same `anchor + ms` fixes when the
step's budget (`within_ms`, else `timing.expect_budget_ms`) OPENS. It opens at
`anchor + ms`, or when the leg reaches the step, whichever is later; before that
the step is matchable but cannot time out. Without this an expect spends its
whole budget across the very gap the document measured — the wait for a message
the flow has not yet been asked to provoke, whether that is a cross-leg send
still dwelling or the step's own ring-to-CANCEL gap — so every captured call
carrying a gap longer than `expect_budget_ms` fails on a faithful document. The
dwell never widens the MATCH, and it never moves the §9.2 tolerance comparison,
which is measured anchor-to-arrival either way.

An anchor may name a BLOCK rather than a step (§6.7 — for an `alt`, branch
scoping makes the block's own id the only legal way to anchor on the branch that
ran). A block settles when it completes, so a dwell measured from one settles
with it.

### 6.9 retransmits

A captured retransmission does not become a step. It collapses onto the step it
repeats, as a count, keyed on leg, direction, message type, transaction, the
INTERVAL between the two AND the BYTES they carry. The count appears on `send`
and `expect` steps alike, since either side of a vantage can be the
retransmitter.

**Only a class that retransmits can repeat, and only inside its transaction.**
A transaction ends at 64·T1 = 32 s — Timer B (RFC 3261 §17.1.1.2), Timer F
(§17.1.2.2), Timer H (§17.2.1), and a reliable provisional's ladder, which
reuses the final response's timers (RFC 3262 §3). Past that envelope, measured
from the EARLIEST emission, nothing is retransmitting, so matching bytes are a
**re-emission**: a fresh event, its own step, its own `observed` coordinate,
seen by every consumer that skips a retransmission. The anchor is the earliest
match either way, so a re-emission never becomes the head of a fresh ladder on
the strength of a near neighbour.

*Amended 2026-08-27 (issue 116), withdrawing "the caller's Timer B bounds it at
the same 32 s".* An **unreliable provisional** — a 1xx above 100 carrying no
`RSeq` — states no repeat relation AT ALL. It rides no timer: the transaction
user re-sends it when it chooses, and RFC 3262 §3 paces only the reliable one.
§17.2.1 names a CAUSE for one re-send, the INVITE going out again, and a cause
is not a pacing, so borrowing the caller's Timer B as this class's envelope
stated a ladder that does not exist. Measured across the corpus, every class
that DOES retransmit sits on T1 — a median gap of 500 ms, the first rung and
nothing else — while this one sits at 3.9 s with a tail past two minutes. So a
platform that rings twice has SENT TWICE, and both copies are events, however
alike the bytes. A callee refreshing its ring every minute (RFC 3261 §13.3.1.1
obliges a non-100 provisional at that cadence) is emitting, not repeating — and
so is one that rings again four hundred milliseconds later.

The one thing that duplicates such a datagram without the platform emitting it
is the CAPTURE STACK, and the extractor's ingest dedup window takes that before
any relation is stated. A tap artefact is not a message, which is a different
question from what this section keys — and it is the ONLY thing deduplication is
for. Nothing here deduplicates a platform's own emissions.

**A repeat is the SAME DATAGRAM, byte for byte.** A count replays N copies of
ONE stored message, so it cannot hold two spellings, and anything a peer could
read differently is a second message. The bound is NECESSARY, never sufficient:
it only ever withdraws a relation the criteria above would otherwise state, so
the transaction key still decides what is a candidate and this decides whether
the candidate is really one message. Three shapes it withdraws, all of them
sharing a transaction: a provisional that grew an SDP offer onto a bare one; two
forked callees answering under one top Via branch, told apart by their To-tags
(RFC 3261 §12.1.1) where the criteria read no dialog at all; and one that gained
`P-Early-Media` (RFC 5009), which authorises early media the first did not. Each
becomes a re-emission with its own step, its own bytes and its own `observed`
coordinate. On an UNRELIABLE provisional the class rule above has already
withdrawn every candidate, so these bite on the reliable one and on the 100
Trying an INVITE ladder draws.

The SUT's own retransmit behaviour is a different subject and is not bounded
here: what this section keys is the CAPTURE's repeat relation.

*Amended 2026-08-27 (issue 120).* That holds for a `send` step, whose emitter is
the scripted peer. On an `expect` step the emitter is the SUT, so the count
states what OUR answerer will put on the wire and is DERIVED, never collapsed:
an INVITE final whose ACK the document holds past T1 obliges the rungs
§13.3.1.4's ladder fires inside that dwell — T1, doubling, capped at T2, inside
the 32 s envelope above. The reasoning is §6.3's, one section over: the two
platforms are different UASs, a captured platform that sat on an un-ACKed final
states its own non-compliance, and replaying that number leaves the repeat a
compliant answerer sends unclaimed — an unannounced repeat the gate then refuses.
Measured over the corpus the derivation and the collapse agree wherever both
speak (53 of 53, no case where the capture held more than the dwell justifies).

*Amended 2026-09-04 (issue 143).* **A count states how long a ladder is; WHO
paced it states whether that count is an assertion.** Four regimes, and the
interpreter asserts each against its own pacer:

| ladder | paced by | asserted against |
|---|---|---|
| a scripted `send` | the document | `retransmits` — it is an instruction the peer obeys |
| an `auto` ACK `send` | the wire (RFC 3261 §13.2.2.4) | the paired final's OWN repeats, one ACK each |
| an `expect` of a paced class | the SUT, on its own T1 | the rungs the class puts inside the ladder's window |
| an `expect` of a drawn class | our own emissions, 1:1 | `retransmits` |

The window is the dwell the closer gave the ladder, or, where nothing closed it,
the run's own end bounded by the 32 s envelope — otherwise the same SUT
behaviour would be judged against the capture on a run that died and against the
RFC on one that did not. One rung over is admitted where it ARRIVED within two
hops of the closer's emission: the emitter stops on the closer it RECEIVES, and
the arrival the wire timestamped says whether the rung was already out.

**The oracle is the RFC on every lane.** The interpreter states what crossed the
wire against what the RFC obliges and must not know which stack is behind the
socket — a lane whose SUT paces differently is a FINDING, which is the point of
the tool. A deviation the project accepts is a tolerance and belongs in the rule
registry, never in here.

This makes a paced-class `expect` claim its ladder whether or not the document
declared one, so an RFC-owed rung is COUNTED rather than refused at the gate as
an unannounced datagram — **which is what the derivation above was for, so the
derivation is withdrawn.** `held-final.ts` is deleted and the
`final-count-derived-from-held-ack` flag with it: an `expect`'s `retransmits` is
once again the plain collapse of what the capture held, recorded evidence that
asserts nothing, and the ladder the run owes is stated in one place. The
derivation had a boundary hack of its own (`owedRungs`, admitting a captured
rung exactly one over) that the interpreter now settles from the arrival the
wire timestamped.

It does NOT collapse where the collapse would leave a lane emitting copies no
rule paces. There the repeat stays one step per emission, carrying its own dwell
and its own `observed` coordinate — which a count, having none, could never
hold. §13.2 names the class this covers and the flag it rides.

*Narrowed 2026-08-27 (issue 116).* Two classes ride no ladder, and the relation
above now answers them differently. The unreliable provisional states no repeat
at all, so the extractor produces none for the generator to decline. The **100
Trying** still does — its copies are DRAWN, one per copy of the INVITE — and a
count on a SENT one is still refused (§14), so the carve-out keeps it as steps.
It also survives for a document whose PRODUCER stated an unreliable-provisional
repeat: a legacy flows document, or another generator.

A peer that re-ACKs each retransmitted final with a FRESH branch mints a
different datagram each time, so those ACKs do NOT collapse: each is its own
step. Ruling Q34 collapsed them, to stop one ladder being encoded twice; the
byte bound withdraws that, because the count §6.3 defines replays copies of ONE
stored ACK and these differ on the wire. The double encoding Q34 guarded against
cannot arise from a step the generator never emits a count on. The generator
identifies repeats by the flows document's `repeat_of`; a flows document that
does not carry the field collapses on `retx` alone and says so. Both fields are
decided together by the extractor and carry the envelope and the byte bound, so
neither can name a repeat the other has released — and since identical bytes
carry an identical branch, the two now name the same messages.

**The count is the document's, and so is the PACING where the document measured
it.** A retransmit count states how many times the message went out;
`retransmit_intervals_ms` states when, one entry per rung, each measured from
the emission before it. Where it is absent — every authored step, and any
generated one whose producer measured nothing — the interval is the one the
message's class prescribes: T1-doubling for an INVITE (§17.1.1.2), the T1/T2-capped
ladder otherwise (§17.1.2.2).

*Amended 2026-08-27 (issue 90), reversing "never a stored interval".* The
premise that a rung's interval is derivable does not survive measurement: across
the captures that declare a 2xx-INVITE ladder the real gap runs from 409 ms to
17 480 ms where T1 says 500. The error is not always cosmetic. 92 ms of it puts a
reliable provisional's rung on the far side of the PRACK that ends it — a
callee retransmitting a provisional after it has answered, which the capture
never held and the protocol does not allow. Ticket 70 rejected storing the
interval for putting "a second, differently-paced meaning on one field"; the
objection is answered by a SEPARATE field, and by the fact that every other
emission in the document already carries its measured `delay`. A rung was the
one emission whose timing was invented.

A stated interval is also what a class-based REFUSAL was ever about: an ACK and
an unreliable provisional are refused a count because "neither the document nor
the RFC states an interval for them". A document that states one has answered
that, so `retransmit_intervals_ms` paces a class that has no ladder of its own.
The generator still does not COLLAPSE an unreliable provisional (§13.2) — that
is a separate ruling about steps, not about pacing. Two classes ride no timer, and each does something else instead: the
ACK's count §6.3 DRAWS from the wire, off the repeats of the final its
transaction names; the unreliable provisional — a 1xx carrying no `RSeq`, re-sent
at the transaction user's discretion, where RFC 3262 §3 paces only the reliable
one — is not collapsed at all.

A repeated provisional is no RFC 3261 §17.2 duplicate — the seam
absorbs a repeated request or a repeated FINAL, never a provisional, which the
client transaction passes up each time (§17.1.1.2) — and it IS a §6.9
retransmission to COUNT on an `expect`: the absorption seam decides what an
`expect` SEES, and the repeated message's own bytes decide what the count is.
Counting arrivals asks nobody to emit, so an `expect` states a count whatever
the class — the guard case is the 100 Trying an INVITE ladder draws. A `send`
is where the count becomes a ladder to run, and there it is stated only for a
paced class or a drawn one.

**A caller-facing provisional beyond the emissions that anchor it is TOLERATED,
never owed.** A captured platform can put more provisionals toward the caller
than it took from the callee — a spare copy of one ring, sub-millisecond apart
with a header stripped, or its own re-send of an unreliable 18x on a refresh of
its own. Both are re-emissions by the bounds above and both keep their own step,
correctly. What neither is, is an obligation: RFC 3261 §17.2.1 re-sends an
unreliable provisional when the INVITE it answers is re-sent, and a replay
re-sends nothing, so a relaying SUT is not CAUSED to emit the surplus. The
generator counts, per leg, the relayed provisional expectations whose `delay`
anchor an earlier one already claimed — or is an earlier one — and stamps that
many `optional`. The step survives, so its content is still asserted when the
SUT does emit it; only the obligation goes.

The surplus is stamped at the END of its run of one status, not on the copy that
caused it: §6.5 releases a tolerated absence when a LATER step on the leg
MATCHES, so only a step of a different discriminator can overtake it, and
members of a run are interchangeable expectations of one status. A whole run is
never stamped — a leg that rang must still ring. This is the ONE `optional` the
generator subset admits (§13.2), and it rides
`provisional-expect-surplus-tolerated`, which the subset gate requires before it
will accept an `optional` in a captured document at all.

**And a caller-facing provisional the capture holds no relay of is DERIVED.**
The deficit is the other half of the same arithmetic. A relaying B2BUA passes
each provisional on, so a document that scripts more emissions than it expects
arrivals gates the SUT on the next message while a datagram it was right to send
goes unmatched. Where the captured platform dropped one on its own account, the
generator gives the emission its arrival.

The derived step COPIES a captured arrival, its `observed` coordinate included
(§6.10): the SUT emits that captured message again, so that message is what both
datagrams are rightly compared against, and the second relay keeps its header
comparison instead of going unreferenced. It rides
`relayed-provisional-expect-derived` (§13.2). Three bounds, and each withdraws a
derivation the arithmetic would otherwise state:

- **One arrival per emission.** Which emission an arrival relays is the delay
  classification's own reading, and that reading is many-to-one — it takes the
  latest emission inside its window, so two emissions milliseconds apart collect
  BOTH their arrivals on the second. A relay emits one datagram per datagram, so
  a second arrival naming a claimed emission belongs to the nearest earlier
  unclaimed one of that status. What is left unclaimed is the deficit.
- **Only a form the SUT was seen relaying.** The derived step copies an arrival
  the capture holds, so the emission it answers must be the SAME MESSAGE as one
  already relayed. A bare ring followed by one authorising early media is two
  different relays and neither is derived: nothing here predicts what a platform
  makes of a message it has not been seen handling.
- **Never past the relaying leg's own final.** A client transaction leaves
  Proceeding when a final arrives (RFC 3261 §17.1.1.2), so a provisional the
  peer emits after that leg has taken its final reaches no transaction user and
  is relayed by nobody. A callee still ringing while its INVITE is cancelled is
  the case the corpus holds.

**And the far side of a relayed in-dialog INVITE the capture holds on one leg
only is DERIVED, where the policy states the replaying platform relays it.** A
leg whose captured record ENDS at the 2xx its peer sent — no ACK, nothing at
all behind it — states nothing about the dialog past that instant: the vantage
stopped seeing it. The near leg goes on, in either direction: its peer sends an
in-dialog INVITE and takes a 2xx nothing on the far leg relays, which synthesis
would read as minted by the platform; or the platform sends an in-dialog INVITE
down the near leg that nothing on the far leg relays into — the far party's own
re-INVITE — and the near peer answers it. A platform that relays such an INVITE
end to end has the far leg on the other end of both, where a document scripting
nothing for it can neither answer nor send: the near leg's expect waits for a
message nobody composes, and the run ends at its budget. So the exchange is
transcribed onto the far leg from the halves the capture holds, in the
direction it ran. For the near peer's re-INVITE: an `expect INVITE` mirroring
the near leg's send, its body compared by content where it carried one; a
`send` of the 2xx the near leg received, headers and body as captured, since a
relayed answer is the far party's own; an auto `expect ACK` mirroring the near
leg's. For the far party's re-INVITE: a `send INVITE` carrying the offer and
the frozen headers the near leg's expect took — a send needs content and the
capture holds no other; the set is §8's tier 3 as the expect holds it, the
hop-by-hop dialog and transaction headers already dropped, so what lands is
end to end except the session-timer pair (RFC 4028 §7.4, §8), which the
platform mints per hop, rides along, and is judged by the confrontation's
withheld-interval rule; an `expect` of the 2xx the near peer sent, its body
stated as the session description the near leg emitted; an auto `send ACK`
carrying what the near leg's expect took. Each derived step copies the near-leg message's
`observed` coordinate (§6.10), since the platform relays that message and it
is what the far-leg datagram is compared against, and each is listed as the
relay runs: a far-leg send before the near-leg arrival it relays into, a
far-leg arrival after the near-leg send that relays into it. The near leg's
arrival is then a relay of the derived send, anchored on it and asserted like
any relayed content (§6.4). It rides `far-side-reinvite-derived` (§13.2), and
it is bounded three ways: the far leg's record must end at its 2xx — a leg the
vantage kept watching that shows no INVITE says the platform did NOT relay,
which a replay must surface; the near-leg answer must be a 2xx, whoever sent
it — a refusal the platform composed is its own, except a 491, which is glare
the replaying platform answers itself (RFC 3261 §14.1) and owes the far leg
nothing; a refusal the near peer sent is relayed like its 2xx would be, but the
far leg's ACK to it is the INVITE client transaction's (§17.1.1.3), a step no
captured message gives a coordinate to, and a 491 there is the near half of a
crossing pair (§14.1) whose other half is the platform's 491 the near-peer
side leaves, so the pair is stated whole or not at all;
`far-side-reinvite-not-derived` names the exchange left alone either way; and the near-leg half must have no
relay origin — a message the far leg's record does hold is already a step. A
far party's offer or answer the near leg holds by shape only (a multipart, an
undeclared binary) is left the same way: no send emits a shape. The first bound
is a proxy for the temporal one: a far leg whose record goes on past its 2xx
and shows nothing at or after the re-INVITE's instant is read as watched, and
nothing is derived for it. A policy that states nothing derives nothing.

A platform running a non-transparency MODE is outside all of it. Such a platform
emits one 18x by design, the replaying SUT is driven the same way, and its legs
are not one for one because nothing is missing. What states such a mode is a
rewrite — a non-180 relayed, SDP stripped — or a managed PRACK, never a COUNT on
its own: an unreliable 18x is relayed as it arrives by a B2BUA that rewrites
nothing, so a platform that dropped one collapsed on its own account and states
no profile for ours to run.

Everything in this section reads the CLASS, never the status. A 180 and a 183
carrying no `RSeq` are one class and take one path — the rewrite above is the
platform turning one into the other, which is a fact about the platform and not
a rule that treats them differently.

### 6.10 observed

```json
"observed": { "leg": 1, "msg": 1, "at_us": 2532 }
```

The step's coordinate in the flows document: `leg` and `msg` index it, `at_us`
is the offset in microseconds from the case's FIRST captured message.

Informative; the interpreter never reads it. It exists because the post-run
confrontation has to pair a run step with the message it is compared against.
The pairing is a LOOKUP, not a consumption, so two steps may name one captured
message where the SUT emits it twice — which is what a derived provisional
expectation does, and what the far side of a relayed re-INVITE derived onto
the leg the vantage lost does on the other leg (§6.9).
Required on a captured document, absent on an authored one.

## 7. timing

```json
"timing": { "expect_budget_ms": 37000, "settle_budget_ms": 37000, "capture_span_ms": 2098 }
```

| field | required | meaning |
|---|---|---|
| `expect_budget_ms` | yes | default assertion timeout for every `expect`; a step's `within_ms` overrides it |
| `settle_budget_ms` | yes | the ceiling on the settle phase (§10) |
| `capture_span_ms` | on a capture | the CASE's span: the last step's `observed.at_us` in milliseconds |

A generated document states both budgets as **64·T1 + a decision margin — 32000
+ 5000 = 37000**, whatever the capture spans. The margin is what the numbers are
FOR. 64·T1 is the slowest SIP deadline a scripted peer can still be waiting
behind (RFC 3261 §17.1.1.2: a request the far side never answers is only given
up on at 32 s), so a budget equal to it makes the runner's give-up race the
platform's: the run books the retransmit ladder it was still riding instead of
the teardown it was waiting for, and which of the two it sees is a coin toss on
scheduling. Held BEHIND that deadline by 5 s, the runner always outlives the
SIP timer it is watching, and a budget that does expire means the message
genuinely never came. An in-dialog request whose ladder runs the full envelope
— the case the margin exists for — therefore fails on the absence, not on the
race.

`capture_span_ms` is the case's span, not the capture file's: the reading a
reviewer or a lane compares a run's own span against. It is NOT a run ceiling —
nothing stops a run at it, and no run needs stopping there, because a virtual
clock spends the span for free and every `expect` carries a budget bounding its
own gap (§6.8). What bounds a run is WALL time, not virtual: the interpreter's
stall detector gives one run 120 s of it, which a real-clock lane replaying a
call longer than that would trip.

## 8. msg, tiers and accessors

The tier model is UNCHANGED:

- **Tier 1, stack-owned**, never stored: Via and branch, Call-ID, From/To tags,
  CSeq numbering, Max-Forwards, Content-Length, Contact host:port, Route sets.
- **Tier 2, role-mapped**, stored symbolically: numbers and domains, as
  positional refs (`{ "pos": "called[0][1]", "form": "trunk-composed" }`, §4.5)
  or, where the plan does not recognize the value, `{ "frozen": "…" }`. A frozen
  ref adds `kind` where the plan classified the ADDRESS without resolving a
  number. The two shapes are exclusive. The party a positional ref names is
  declared once, in the identity registry (§8.5).
- **Tier 3, frozen**, stored verbatim: everything else, in wire order.

`RSeq` splits by direction: a **send** freezes the captured value (the scripted
endpoint must put a concrete number on the wire for RAck translation), an
**expect** states existence only via `headers-present` — the value is
stack-owned per leg (RFC 3262 §3) and the confrontation's `rseq-stack-owned`
rule judges the number.

The body descriptors split the same way (RFC 3261 §20.11 Content-Disposition,
§20.12 Content-Encoding, §20.13 Content-Language, §20.24 MIME-Version): a
**send** freezes them with the rest, an **expect** freezes none of them when
its captured message carries no body — they describe octets that are not
there.

```json
"msg": {
  "method": "INVITE",
  "ruri": { "pos": "called[0][0]", "form": "trunk-composed" },
  "from": { "pos": "caller", "form": "private" },
  "to":   { "pos": "called[0][0]", "form": "trunk-composed" },
  "headers": [ { "name": "Allow", "value": "INVITE, BYE, CANCEL, ACK" } ],
  "headers-present": [ "session-expires" ],
  "body": { "ref": "resources/uac1_0_0.sdp", "rewrite": ["c=addr", "m=port"] }
}
```

| field | on | meaning |
|---|---|---|
| `method` | requests | discriminator |
| `status`, `reason`, `cseq-method` | responses | discriminator |
| `cseq` | auto steps only | the CSeq number (§6.3) |
| `ruri`, `from`, `to` | dialog-opening INVITE sends | tier-2 refs |
| `headers` | any | tier-3 frozen list, wire order |
| `headers-present` | expects | existence checks |
| `body` | any | §8.3 |

`headers` and `headers-present` are stated on an `expect` regardless of `check`.
Under `assert` they are matched; under `record` they are the recorded value.

The v2 match vocabulary is unchanged: `headers-absent`, `remote-target` with its
`ignore-params` list, body matchers by kind, `early` ids for UAS-simulated
forking, and the `overlap` declared race.

### 8.1 Accessors — AUTHORED ONLY

A header value may name a value the RUN produced rather than one the capture
held. Four namespaces, and the order between them matters.

**Leg accessors are primary.** They resolve against runner dialog state:

```
${leg:<leg-id>.call-id}        ${leg:<leg-id>.cseq.local}
${leg:<leg-id>.local-tag}      ${leg:<leg-id>.cseq.remote}
${leg:<leg-id>.remote-tag}     ${leg:<leg-id>.rseq}
${leg:<leg-id>.remote-target}
${leg:<leg-id>.route-set}
```

They exist because asserting a dialog's own tags, composing a header out of
them, or routing through a proxy needs exactly this dialog state.

**Early-dialog accessors** read ONE fork of a forking leg:

```
${early:<early-id>.tag}        ${early:<early-id>.rseq}
```

The id is the `early` a step carries (§6.1). A leg accessor is single-valued and
a leg ringing two forks has two To-tags and two RSeq spaces (RFC 3261 §12.1.1,
RFC 3262 §3): `${leg:<id>.rseq}` publishes the last RSeq the leg SIGHTED, on
either side of the wire, and the early namespace publishes the fork's own.
`.tag` is minted before the run speaks and always answers; `.rseq` answers once
a reliable provisional has ridden that fork. Lint refuses an id no step
declares, and one two legs declare — a leg owns its own fork tag space, so the
same id on two legs is two dialogs and the accessor names neither.

**Step accessors are secondary**, for the rare specific-message case:

```
${step:<step-id>.header.<name>}   ${step:<step-id>.status}
${step:<step-id>.cseq}            ${step:<alt-id>.branch}
${step:<step-id>.rseq}
```

**Number accessors** resolve against the identity registry and the lane's
binding of it:

```
${num:<identity-name>:<dial-form>}
```

Both halves are required. The name is an `identities` entry (§8.5) and the form
is one the entry declares — a lane can only bind a form the numbering plan
resolved, so lint refuses one the registry does not state. This is what lets a
number-bearing header be composed rather than frozen: `Refer-To`,
`Referred-By`, `P-Asserted-Identity`, `Diversion`, `History-Info` and `Contact`
all carry numbers the plan owns, and freezing them would strand the header on
the lane the capture was cut from.

A `Refer-To` composed from `${leg:…}` state has no end-to-end consumer today:
REFER with `Replaces` is out of scope (§0.1), and it is the `Replaces` form that
would read a leg's dialog identity back out of the header. The accessors that
compose one stay defined — the header is a URI like any other and the namespace
does not shrink around a single use.

A body is only ever a match target, never extracted and resent. A step accessor
must point backwards — a value the run has not produced yet is not a value —
and it obeys §6.5's branch-scoping rule. `.branch` names an `alt` node and
nothing else, and is readable only AFTER that alt has completed.

An accessor is looked for in **every string a document carries**: header names
and values, `headers-present`, tier-2 refs (positional and frozen alike), body
refs and content types, check fields and values, an `inject`'s action and
target. There is no position where a `${…}` is treated as literal text.

**Arithmetic is structural, not an expression language:**

```json
{ "from": "${step:s7.cseq}", "delta": 1 }
```

`{ from, delta }` and nothing else is computable. An accessor that needed more
would be a program, and this document does not run programs.

Accessors resolve against runner state, never against document text. A compiled
document is immutable and shared across call instances; a leg accessor is what
makes per-call identity substitution possible without re-parsing anything.

### 8.2 Tier data is NORMATIVE DATA, not prose

The tier-1 omission list and the per-message From/To/Contact handling are
**normative data exported by the Rust schema owner**, not prose each generator
reverse-engineers. `pivot-schema tiers` emits it as JSON; the TS generator and
the Rust interpreter consume the same export, and compact-form header identity
comes with it, enumerated from `sip-message`.

The same applies to the multipart per-part HANDLING registry (§8.3). The export
carries the GENERIC arm only: `application/sdp`, the eCall and `+xml` and
`text/*` freeze rules, and the flag-when-unrecognized default. A deployment that
rewrites numbers inside its own body format registers that content type in its
own overlay.

### 8.3 Bodies and multipart

A single body on a SEND references a resource file, and the registry decides
what rides beside the ref: `rewrite` tokens where the body is rewritten
(`{ "ref": "resources/…", "rewrite": ["c=addr", "m=port"] }`), or `mode`
(`frozen`) plus the captured `content-type` where it replays byte-exact. A
resource file holds BYTES; whether they are text is a fact of the bytes, never
a declared mode, and no rule names a media type to say so (ADR-0035).

**The tokens name what the LANE may rewrite, not what it must.** They are
applied through the lane's media booking, and a lane that exercises no media
runs a VERBATIM booking: every session description then rides byte for byte as
stored, `c=` address and `m=` port included, and the tokens still label the
body `application/sdp`. Which plane a run took is stated in its run
configuration (`"media": "verbatim"`; absent reads as `rebooked`), so a reader
of the bundle knows whether a relayed description can be held to the capture.

**A `content-type` is stored VERBATIM, its MIME parameters included**, because
emission writes the stored value back as the message's own `Content-Type`
(`message/sipfrag;version=2.0` replays under its version, not without it). The
ONE value left unstated is bare `application/sdp` on a rewritten body, which
render derives exactly, so the stored and derived values cannot drift; an SDP
body whose captured type carried parameters states them and replays under them.

A single body on an EXPECT is asserted by SHAPE or by CONTENT, and the
registry decides which. A message that carried no body states
`{ "mode": "absent" }`, and that IS the assertion. Every body the registry
freezes is stored as a resource and asserted by content, whatever its bytes
hold, in the same form the send side stores it, and so is every SDP:

```json
"body": { "ref": "resources/uas1_r3_0.xml", "mode": "frozen",
          "content-type": "application/example+xml", "compare": "xml" }
"body": { "ref": "resources/uas1_r0_0.sdp", "rewrite": ["c=addr", "m=port"],
          "compare": "sdp" }
```

`compare` says how the received body is held against the file — `exact`, byte
for byte, which is what an absent `compare` means; `xml`: both sides as XML
text after ONE normalisation (the XML declaration dropped, whitespace-only text
between two tags removed wherever it sits, mixed content included, leading and
trailing whitespace trimmed) and nothing else — no attribute reordering, no
entity work; or `sdp`: both sides as a session description — the session
section then the media sections by position; within a section lines compare
as a multiset (attribute order erased); `o=` sess-id and sess-version are
masked always, and every field a `rewrite` token of the expect names is
masked where the run's media plane rebooked it. The mask states exactly what
the render writes: `c=addr` masks the address of a `c=IN IP4` line and no
IP6 line; `m=port` masks the port of an `m=` line where it is non-zero, its
`/count` kept, and no port-0 stream; `a=rtcp` is never written and never
masked. Nothing else. **The tokens name WHICH fields are lane-owned; the
run's media mode says whether they were applied**: on a `verbatim` run the
tokens mask nothing, so a `c=` or an `m=` the system alters is a difference,
and the system relays the description byte for byte, so two descriptions the
structure cannot tell apart — an attribute reordered inside a section, a
bare-LF line ending, trailing whitespace, an inner blank line — must be the
same bytes; on a `rebooked` run those are erased, because the render
re-assembles the line endings there. The generator stores every expected SDP
this way and leaves `compare` unstated on every other body; `xml` is authored,
where a document has to say that a re-serialised body is the same body, and
`{ "mode": "sdp-present" }` stays an authored shape for a document that
asserts presence alone. `compare` on a SEND describes a check nobody runs and
lint refuses it (`body/compare-on-send`); `sdp` on a body whose stated content
type is not `application/sdp` names a fold that cannot read the file
(`body/compare-sdp-type`).

The two halves of the assertion run at two times, and only one of them
depends on `check`. The interpreter gates on PRESENCE alone, and only under
`check: assert` — a body must have arrived, whatever it holds — so a differing
body never abandons the call: the message is answered, the dialog walks to its
captured teardown, and the post-run confrontation states the difference as a
`body` record (`body:<type/subtype>:<scope>`, the expected text and the
received text side by side, both verbatim), which a lane's rule lists then
classify. Under `sdp` there is one record per differing line key,
`body:sdp:<section>:<line>:<scope>` — `<section>` is `session` or `m<i>`
(0-based, wire order), `<line>` is `<type>=` for a session line or `a=<name>`
for an attribute, the four direction attributes under one key `a=direction` —
each side carrying that section's verbatim lines of that key in wire order,
one element per line the way a header record carries one per value; a side
that is empty or no session description is one `document:sdp` record with
both texts whole; a media section on one side only is one `m<i>:section`
record with that side's lines; two descriptions a verbatim run carried as
different bytes with the structure equal are one `document:bytes` record
with both texts whole. A reception carrying NO body where content is asserted is confronted
as the empty text; under `check: assert` the interpreter also refuses it, and
under `check: record` — every generated expect — the confrontation's record is
the only statement of it.

**A frozen body compares byte for byte.** The confrontation holds the received
bytes — the recording keeps them as they crossed the socket (§14 item 12) —
against the resource file's; `xml` and `sdp` decode both sides as UTF-8 first
and apply their fold, and a side that is not UTF-8 under a text compare is a
difference. A probe's sides are shown as the text they are where the bytes are
UTF-8, standard base64 where they are not.

A multipart body on an EXPECT is stated PART BY PART where extraction handed
the parts over — the same `multipart` form the send side stores, every part a
resource, an SDP part under its `rewrite` tokens with `compare: sdp`, any
other part `frozen` — and the confrontation locates the received parts by the
recording's own `body` layout and compares them to the expectation's by
position: one record per differing part under that part's `compare`, one for
a part-count mismatch. A part's entity headers are not compared. Where
extraction handed no parts over, the expect falls back to the shape
`{ "mode": "multipart-present" }`, and the interpreter gates the reception's
container type either way.

Multipart bodies reference DECOMPOSED parts:

```json
"body": { "multipart": { "content-type": "multipart/mixed", "parts": [
  { "content-type": "application/sdp", "ref": "resources/uac1_0_0.sdp",
    "rewrite": ["c=addr", "m=port"],
    "content-id": "<offer@example.invalid>",
    "headers": [{ "name": "Content-Disposition", "value": "session" }] },
  { "content-type": "application/EmergencyCallData.eCall.MSD",
    "ref": "resources/uac1_0_1.bin", "mode": "frozen",
    "content-id": "<user1@ims.example.net>",
    "headers": [{ "name": "Content-Transfer-Encoding", "value": "binary" },
                { "name": "Content-Disposition", "value": "By-Reference" }],
    "cid-linked": ["call-info"] }
] } }
```

**Splitting a multipart body is EXTRACTION's job.** The flows emitter splits it,
so no consumer owns MIME; the pivot references files that already exist.
`cid-linked` is COMPUTED from the part's `Content-ID` plus the message's header
list.

A part on an expect may state `compare` with the meaning a single resource
body gives it (`exact` when absent, `xml`, `sdp`); on a send it is ignored.

A part states its own ENTITY BLOCK. `content-id` is its `Content-ID` value with
the angle brackets the wire wrote (RFC 2045 §7), and `headers` is every other
entity header it carried, verbatim and in wire order —
`Content-Transfer-Encoding`, `Content-Disposition`, whatever else the part
states. `Content-Type` and `Content-ID` have their own fields and are never
repeated in `headers`. Both are stated where the capture carried them and absent
where it did not. A part that `cid-linked` names but that states no
`content-id` is REFUSED at emission, because the header referencing it would
resolve against nothing (RFC 5621 §3).

**A body is emitted as the document holds it.** Every part rides byte-exact —
its payload, its `content-id`, its `headers`, in the order stated — and the ONE
content the tool may rewrite is SDP, direct or as a part, because replay rebooks
addresses and ports. Nothing else in a body is edited, ever.

**Both tokens read one source: the lane's own media booking** (`legs[].media.rtp`,
§5). `c=addr` writes the lane's media address into every connection line;
`m=port` writes into the i-th ACTIVE `m=` line of the leg the port the lane
booked for that leg's i-th stream, keeping the rest of the line — the media
kind, a `<port>/<count>` pair count, the transport, the format list — byte-exact
(RFC 4566 §5.14). A port-0 stream is one the capture rejected or disabled
(RFC 3264 §5.1): it keeps its zero, books nothing and takes no index. A booking
is held per `(leg, stream)` for the whole run, so a re-offer and a retransmission
carry the port their first emission did. A lane without media runs a verbatim
booking that answers neither token (§8.3): the same document then emits every
session description as stored, and the run configuration says so.

What the document does not store is regenerated rather than replayed, and the
list is ONE item long: the container's `boundary`. The container type is held
with that parameter — and no other — stripped, so the boundary is derived at
emission from the parts while `type=`, `start=` and anything else the container
carried ride through. A part is framed `Content-Type` first, then its
`content-id`, then its stated `headers` in order.

So the body on the wire is the captured body BYTE FOR BYTE once the regenerated
boundary and SDP's rewritten lines are substituted back, and nothing else may
differ — not a MIME parameter, not an entity header, not a delimiter's CRLF.
That is asserted, on real traffic, by the multipart rung.

Per-part HANDLING is a content-type registry, matched on the BARE type — the
media type lowercased with its parameters stripped — so a stored parameter
never changes how a part is handled:

| part content-type | handling |
|---|---|
| `application/sdp` | rewrite `c=` and `m=` |
| known non-SDP | freeze (`frozen`, text or bytes alike) |
| unrecognized | freeze AND flag when the payload carries number-like digits |

The unrecognized arm is the point: a missing handler is a decision owed, not a
silent freeze.

### 8.4 Resource naming

A resource ref is `resources/<actor>_<n>_<part>.<ext>` on a send — the actor,
its ordinal over the messages it emits, and the part index within that message
— and `resources/<actor>_r<n>_<part>.<ext>` on an expect, the ordinal counted
over the messages the actor expects. Every message takes an ordinal whether or
not it carries a body, and the two counters are separate, so a send and an
expect of one actor never name one file. The name is **step-index-free** (v2
friction H1) — renumbering the flow, inserting a step, splitting a deviation,
none of it renames a file on disk.

### 8.5 The identity registry

Every number and domain the document names is declared ONCE, at the top level:

```json
"identities": [
  { "name": "caller", "kind": "external-caller", "observed": "0009001",
    "forms": ["private"] },
  { "name": "called-0-0", "kind": "site", "observed": "+33000900004",
    "forms": ["e164"], "catalog": { "class": "site" } },
  { "name": "transferee", "kind": "site", "forms": ["e164"] }
]
```

| field | required | meaning |
|---|---|---|
| `name` | yes | unique within the document, free of `.` and `:`. The only handle onto the identity |
| `kind` | yes | open plan token classifying it (a caller class, a catalog class, `unknown`) |
| `observed` | no | on a capture, the anonymized value it carried; on an authored document, optional and informative only |
| `forms` | no | open dial-form tokens the plan resolved, and the forms `${num:…}` may ask for |
| `catalog` | no | what the number catalog says about the value, when a catalog answered |

The no-real-number discipline is about REAL calls: `observed` is capture
provenance, and it is on a CAPTURED document that it must be the anonymized
value. An authored test may state numbers freely — nothing lints it — and the
symbolic way (`${num:…}`, name-only identities) is encouraged rather than
enforced, because the lane binds the name and never this field.

**The registry names and classifies; it never binds.** Which real number a name
becomes is a per-lane translation the driver performs (§4.3): a mock lane's
allocation and a provisioned backend's leased number are different numbers for
one entry. A document that embedded the value could only ever be replayed on the
lane it was cut from — which is exactly what froze the transfer family before
this section existed.

Three sites name an entry, and nothing else carries an identity:

- `calls[].attempts[].callee.identity` — the dialed party;
- `actors[].identity` — the actor's own, the caller's in practice;
- `${num:<name>:<form>}` — anywhere an accessor is allowed (§8.1).

**One encoding, captured and authored alike.** A generated document synthesizes
the name from the party's tier-2 position: `caller`, and `called-<branch>-<position>`
for each attempt. A document declaring several calls MUST qualify the name with
the call id (`c2-called-0-0`), as position tokens do, and lint refuses a bare
position name there (`id/identity-unqualified`) for the same reason it refuses a
bare `called[b][s]`: the name would position one of two chains. The capture
generator emits one call per case, so what it emits is always the bare form. An
authored document may also name a party the chain never dials — a transferee the
platform must refuse to reach — and gives it whatever name reads best; a name
that positions nothing needs no call id.

`name` and every `forms` entry are spelled inside `${num:<name>:<form>}`, so
neither may carry `:`, `{` or `}`.

The registry does NOT loosen the subset gate. A capture may carry the registry,
because a capture observed those numbers; it may not carry a `${num:…}` in a
header value, because substituting there is an inference (§13.1). Captured
number-bearing headers stay frozen text.

## 9. checks

One check vocabulary everywhere, borrowed from the upstream `e2e-model`:

```json
{ "field": "to.tag", "op": "eq", "value": "${leg:A.remote-tag}" }
```

| field | required | meaning |
|---|---|---|
| `field` | yes | field selector. Open token: `from.userInfo`, `header(P-Asserted-Identity)`, `body`, `body.b64`, or a deployment observable's name in a postcondition |
| `op` | yes | `eq`, `regex`, `exists`, `absent` |
| `value` | with `eq` / `regex` | a literal, a regex, or a string carrying `${…}` accessors |

`body` observes the message body as text where its bytes are UTF-8 and as
standard base64 where they are not, chosen by the bytes alone; `body.b64`
observes it as base64 always — the byte-exact assertion an author writes for a
body that is not text.

`exists` and `absent` take no value, and `eq` / `regex` require one; lint refuses
either mismatch.

Checks appear inline on an `expect` (asserting over the matched message) and in
`postconditions` (asserting over what the run left behind). Both are
authored-only.

### 9.1 Check classes and lane scoping

**The document carries facts. What a fact costs on a given lane is the lane's
decision, and it is taken outside the document.**

Some of what a document asserts is protocol — a status, a header SIP defines, a
dialog identifier — and holds on anything that speaks SIP. The rest is one
platform's spelling: the headers it stamps on its own egress, the words its CDR
writer uses. Replayed on another platform, the second kind fails for saying
nothing about the system under test, and a document written to survive that
would have to drop the fact. So the fact stays, NAMED.

**A class is stated, never inferred.** Two members, closed:

| class | what it reads |
|---|---|
| `origin-platform-header` | a header value only the origin platform emits: a family of its own (`P-Charging-Vector`, `P-Identifier`, `P-Orig`), or a value the capture never shows reaching it |
| `cdr-vocabulary` | the origin platform's CDR record vocabulary: event names, disposition words, field spellings |

Two sites carry one:

- a `checks` entry, inline or in `postconditions` (including the CDR block's):
  `{ "field": "events", "op": "regex", "value": "InviteReceived", "class": "cdr-vocabulary" }`;
- a frozen header inside `msg.headers`:
  `{ "name": "P-Charging-Vector", "value": "…", "class": "origin-platform-header" }`,
  which is what scopes an `assert`-mode expect at header granularity instead of
  all-or-nothing per message.

An unclassified check gates on every lane. `headers-present` carries no class:
existence is not a vocabulary.

The generator states the class from two readings, and the interpreter infers
none: the deployment's own header families, and — §6.4 at header granularity —
an asserted value no capture-side `send` of the document carries. A value the
capture never shows reaching the SUT was minted by the origin platform, so it is
that platform's spelling whatever the header's name; the same header relayed
byte-for-byte stays protocol and gates.

**The rule a run applies, and it is the only one:**

> when the run's lane is not `case.origin_lane`, a CLASSIFIED check is
> evaluated, recorded and does NOT gate.

A document stating no `origin_lane` downgrades nothing. A run configuration
states its own lane and may override a class outright, in either direction — a
lane that shares the origin platform's CDR vocabulary without sharing its name
states `gating`, a lane replaying its own document under a foreign header
profile states `informative`:

```json
{ "lane": "upstream-fake",
  "check_scoping": { "cdr-vocabulary": "gating" } }
```

Anything more conditional than one word per class is a pre-processing pass over
the document, in the driver, before the run. It is never interpreter smarts:
the interpreter holds one comparison and no vocabulary.

**A downgrade is not a skip.** An informative check is evaluated exactly as a
gating one is, and a failing one lands in the verdict's own `informative`
section with its class beside it, so a reader sees what the other lane's
vocabulary would have said. The run's `status` stays `passed` or `failed`,
computed from gating checks alone; there is no third status value.

### 9.2 Timing tolerance — the run's window, not the document's

**A `timer_linked` dwell (§6.8) is a fact about a SYSTEM timer, and a system's
timers are its own.** One platform arms a whole second where the capture measured
15 139 ms; a platform under real load fires a few hundred milliseconds off. The
document keeps what it measured — that is the fact — and the RUN states the
window it accepts around it:

```json
{ "lane": "deployed-backend", "timing_tolerance_ms": 700 }
```

The window reads ONE thing: how long the system's own timer ran before it
emitted, measured from the dwell's anchor to the arrival, on an `expect` that
declares `timer_linked`. It never widens anything else:

- not ORDER — a message that arrives in the wrong order is out of order whatever
  the window says;
- not a COUNT — an absence is an absence, and a `retransmits` ladder (§6.9)
  counts messages rather than measuring time;
- not a `send`'s own dwell, which the runner sleeps rather than observes;
- not an `expect`'s `within_ms`, which is how long the run WAITS before giving
  up, not what it judges the arrival by.

**Every reading is reported, absorbed or not.** The verdict carries one entry per
timer-anchored expect the run completed — declared, observed, the signed
difference, and the window that covered it — so a window never swallows a delta
silently (the §9.1 rule again: evaluated and recorded beats invisible). Stating
no window accepts the declared value exactly, and that is what a virtual-clock
lane measures: on a paused runtime the system's timer fires where the document
says. A window stated there is honoured and reported all the same, just rarely
exercised; its consumer is the real-clock lane.

**A dwell the LANE cannot arm is refused, not rounded.** Where the lane's own
grain is coarser than the document's dwell — a route decision that speaks whole
seconds against a `no_answer_ms` of 15 139 — the driver arms the nearest value it
can and the difference rides this same window. A difference the window does not
cover refuses the run before it dials (§14): a ring half a second short of what
the document measured proves a different thing than the document does.

## 10. The settle contract and postconditions

**After the last flow node the runner ALWAYS runs a settle phase.** It waits
until every scripted dialog is terminal AND the system reports no active call
AND the CDR expectation is met, bounded by `timing.settle_budget_ms`.

A scripted dialog is terminal only once the transactions it holds are. A
non-2xx final a scripted leg sent to an INVITE keeps the run open until the
ACK the system owes it on the INVITE's branch (RFC 3261 §17.1.1.3) or Timer H
from the final's first emission (§17.2.1), whichever comes first; that ACK is
recorded as the transaction's own closer, never as a datagram after the flow.
Past Timer H the transaction is gone and the missing ACK is the system's
failure, `final-unacknowledged`: the run settles and states it.

**Failing to settle is always test failure. There is no soft mode.**

A run whose SCRIPT ended because it could not go on (§14) settles by the same
three conditions, whatever its polarity. "Every scripted dialog is terminal" is
then what the generic close discharges, node by node, inside the same budget; the
flow's own half is done by ruling, so the abandoned nodes raise no
`flow-incomplete`.

```json
"postconditions": {
  "cdr": { "count": 1,
           "checks": [ { "field": "disposition", "op": "regex", "value": "^(ANSWERED|CANCELLED)$" } ] },
  "checks": [ { "field": "sip_transactions_orphaned_total", "op": "eq", "value": "0" } ]
}
```

| field | meaning |
|---|---|
| `cdr` | either `{ count, checks? }` or `{ absent: "<reason token>" }` |
| `checks` | deployment observables — metric names, store contents — checked at settle and NEVER mid-flow |

**CDR checking is default-on.** A document that states no `cdr` is refused by
lint; a document with no CDR oracle says so with an open reason token, which
makes the gap greppable instead of invisible. Generated documents state
`{ "absent": "capture-carries-no-cdr" }`: a capture shows what crossed the wire
and nothing about what was billed.

HTTP-body assertions deliberately do not port from the Rust tests. The routing
adapter is configured by the document (driver-compiled, §4.3) and asserted at
unit level; a ported test asserts end-to-end SIP behavior plus CDRs.

## 11. deviations

Named, reviewed, grep-able non-compliance the replay must REPRODUCE — captured
from a peer, or deliberate in an authored test. Each entry names the step or leg
it applies to.

```json
"deviations": [
  { "id": "d1", "kind": "verbatim-emission", "leg": "A", "step": "s1",
    "preserve": ["header-order", "casing"] },
  { "id": "d2", "kind": "cseq-override", "leg": "A", "step": "s22",
    "value": { "from": "${step:s11.cseq}", "delta": 3 } },
  { "id": "d3", "kind": "suppress-auto", "leg": "B", "step": "s12" },
  { "id": "d4", "kind": "malformed-header", "leg": "B", "step": "s8",
    "header": "Refer-To", "preserve": ["header-value"] }
]
```

**Every violation lives here and nowhere else.** There are no inline tier-1
overrides on a step: the flow reads as intent, and what breaks the rules is
greppable in one block.

`kind` is an open token — the set grows with the corpus, and a document naming a
kind a given interpreter does not implement must still parse so lint can say so.
The kinds with a stated payload:

| kind | payload | meaning |
|---|---|---|
| `verbatim-emission` | `preserve` | the message goes out exactly as stored. The trigger step is sought among SCRIPTED sends only |
| `raw-order` | `preserve` | header order survives emission |
| `cseq-override` | `value` | the CSeq to emit: a number, or `{ from, delta }` relative to a CSeq the run observed |
| `suppress-auto` | `step` | the named auto step is withheld. A withheld ACK is a deviation pointing at the step, not a hole in the flow |

Two payload fields are shared rather than kind-specific:

| field | meaning |
|---|---|
| `header` | the header the deviation targets, canonically named. A deliberate content-level malformation breaks ONE header's grammar in an otherwise compliant message; without it the entry points at the step and an interpreter cannot tell which of its own renderers to stand down |
| `races` | the step id this deviation's step raced with, where the non-compliance IS the race |
| `retransmits` | repeats the capture shows, where the non-compliance IS the repeat. Distinct from `flow[].retransmits`, which counts a protocol-legal retransmission on the step itself |

Lint checks the payload of the kinds the format defines — a `cseq-override`
without a `value` overrides nothing, and a `suppress-auto` naming a scripted
step withholds nothing, since the interpreter never composed that message. It
checks nothing about a kind it does not know, because `kind` is open. A
`verbatim-emission` naming an auto step is NOT refused: such a step stores what
any step stores (§6.3), so there is a block to preserve.

Three things the defined kinds do NOT change:

- a `verbatim-emission` step still carries the run configuration's injected
  headers. They are appended AFTER the stored block, so the preserved order is
  the stored one, and a document header of the same name wins — a lane artifact
  never displaces the choreography;
- a `cseq-override` is not a one-message edit. The value it states BECOMES the
  leg's sequence number, and the leg's subsequent requests continue from it
  (RFC 3261 §12.2.1.1), because a leg that jumped back would number two requests
  alike;
- the `preserve` token set the interpreter honours is CLOSED: `header-order` and
  `casing`. Tier-1 is never stored (§8), so there is no absolute interleaving to
  restore and no token that could ask for one; a token outside the set is
  refused rather than ignored, since a property nothing holds would still read
  as held.

A held automatic is NOT a deviation: the hold is the auto step's `delay`, and
the repeat count is `retransmits` on the step that was repeated.

### 11.1 rfc_violations

A `deviations` entry changes what an emission LOOKS like. Some non-compliance
changes nothing about the bytes: the message is well-formed and it is WHEN or
in what context it is sent that breaks the rule — a UAS answering 200 to an
INVITE it has already taken the CANCEL for (RFC 3261 §9.2). A replay reproduces
that by running the flow unchanged, so a deviation kind is the wrong shape for
it; declaring one asks an interpreter to emit something it has no way to emit
differently.

```json
"rfc_violations": [
  { "rule": "no-200-after-cancel", "step": "s11", "emitter": "uas1" }
]
```

| field | meaning |
|---|---|
| `rule` | which rule is broken. CLOSED vocabulary |
| `step` | the flow step whose message breaks it — the anchor, so a reader lands on the datagram |
| `emitter` | who emits it: an `actors` id, or `sut` |

`rule` is closed where a deviation `kind` is open, and the difference is
deliberate: a kind an interpreter cannot execute still parses so lint can say
so, while a rule nothing can DECIDE off the wire is a claim nothing can hold a
run to. The vocabulary grows one detector at a time, and a member arrives with
its detector:

| rule | what it decides |
|---|---|
| `no-200-after-cancel` | RFC 3261 §9.2 — a UAS that has taken a CANCEL for an INVITE answers 487, never 2xx |
| `unacked-reliable-provisional` | RFC 3262 §4 — a UAC that took a reliable provisional (§3: an INVITE offering `100rel`, answered by a 101-199 carrying BOTH `Require: 100rel` and an `RSeq`) answers it with a PRACK whose `RAck` names it (§7.2) |
| `no-ack-to-dialog-creating-2xx` | RFC 3261 §13.2.2.4 — a UAC that took a dialog-creating 2xx to its own INVITE answers it with an ACK on that dialog. One ACK is owed per 2xx RECEIVED (the core sends it, not the client transaction — §17.1.1.3), keyed on the INVITE's CSeq number and the To tag, so a retransmission ladder is one obligation and a fork's 2xx is its own |
| `no-cancel-after-final` | RFC 3261 §9.1 — a UAC CANCELs a client transaction still in flight. Once a final has landed the transaction is completed (§17.1.1.2) and the CANCEL names none the server holds, so it draws a 481 (§9.2) and changes nothing. Conservative on the pairing's own terms: a final observed just before the CANCEL may have crossed it in flight, so only a CANCEL sent after the emitter's OWN ACK for that final (§17.1.1.3, same branch) is charged |

The detectors are `sipflow --rfc-census`
(`crates/sip-pcap/src/rfc/`), and each one's exact conditions, its
conservatism and its corpus numbers live with the census report.

**Emitter attribution decides gating.** A violation a SCRIPTED PEER emits is
what the case exists to reproduce: the run lists it prominently in its verdict
and never gates on it, and nothing about the run turns red for reproducing the
behaviour it was written for. A violation the SYSTEM UNDER TEST emits is a
defect of the thing being tested, and it gates. Until a detector decides the
rule off the wire, a run that meets a SUT-emitted entry refuses by name rather
than passing a claim nothing verified.

`races` on a deviation stays exactly what it is: informative race-existence
metadata. A race with no violation is still a fact worth keeping, and nothing
that gates reads it.

An entry that lands on no step, or names an emitter the document does not
declare, is refused (`ref/violation-step-unknown`,
`ref/violation-emitter-unknown`): a violation nothing can be attributed to
gates nothing and points nowhere.

### 11.2 must_fail

An `rfc_violations` entry states what the SOURCE broke and lets the replay
reproduce it by running the flow unchanged. Some source non-compliance cannot be
reproduced at all, because what answered it was the SOURCE PLATFORM and the
replay's platform is this one. The withheld ACK is the case: where a peer never
ACKed a dialog-creating 2xx, the source relayed an ACK that never came, and this
platform answers such a 2xx LOCALLY (RFC 3261 §13.2.2.4, the behaviour §13.2
already turns into a lane adaptation). So the run emits a datagram the capture
never carried, on a leg where the document states no step for it.

Replaying reality means the run FAILS. What the document adds is the failure it
owes.

```json
"must_fail": [
  { "failure": "unexpected-ack", "step": "s14",
    "derived_from": "no-ack-to-dialog-creating-2xx" }
]
```

| field | meaning |
|---|---|
| `failure` | what the run must produce. CLOSED vocabulary |
| `step` | the flow step the failure happens at or immediately after: the anchor, so a reader lands on the datagram the divergence turns on |
| `derived_from` | the §11.1 rule the SOURCE broke, whose violation predicts this failure |

**A document that declares a `must_fail` is a NEGATIVE case.** Its run passes
only by failing as declared: every declaration produced, or the run is a failure
like any other. That is what makes a corpus of negative cases a standing proof
that detection is alive: a case that could go green by behaving well would prove
nothing, and an exclusion proves less than that.

**A declaration is an INCLUSION, not an exhaustive prediction.** A negative case
replays a capture the replay is KNOWN to diverge from, and past the divergence
the tail diverges with it — our ladder replaces the recorded one, a step nothing
answers times out, a datagram nobody scripted arrives. So a run that produced
every declaration CARRIES the wire divergences beside them, and fails on
everything else. The two classes, and the boundary is not negotiable in either
direction:

| class | what it says | on a negative case |
|---|---|---|
| WIRE | the replay and the capture disagree about packets: a datagram nothing expected, one no armed expect matched, one after the flow ended, an expect the tail never satisfied, a retransmission count, a timer-anchored dwell | CARRIED, at or after the declared divergence |
| STRUCTURAL | everything else: the call did not settle, was not billed or did not end; a postcondition or a gating check did not hold; the run could not emit what the document states; the lane, the document or the run's own machinery failed; the system under test broke a rule (§11.1) | FAILS the run |

A carried divergence is never dropped. It MOVES to the verdict's `tolerated`
list, as a matched declaration moves to `must_fail`, so `failures` can be empty
on a green negative case while a reader still sees everything else the replay
diverged on.

Two things the split does not soften. A wire failure BEFORE the divergence fails
the run: up to the anchor the replay was still following the capture, so a
failure there is a defect of its own. And carrying is ALL OR NOTHING — one
structural failure and every wire failure stays in `failures` too, because a red
run's `failures` must be the whole account rather than a filtered half of it.

`failure` is closed where a deviation `kind` is open, and for §11.1's reason
read from the other end: a failure nothing can PREDICT off the capture is a
claim nothing can hold a run to. The vocabulary grows one prediction at a time,
and a member arrives with the derivation that decides it.

| failure | what the run must produce |
|---|---|
| `unexpected-ack` | an ACK this platform sends to the dialog-creating 2xx the anchor step EMITS, which no step states because the source never carried one. The ACK to a 2xx is the acknowledging peer's own, relayed (RFC 3261 §13.2.2.4), so where the caller never ACKs the platform's own §13.3.1.4 give-up composes one before its teardown BYE — on an offer-carrying INVITE, whose ACK owes no answer body; a delayed-offer dialog gets the BYE alone and draws no such datagram. The derivation today still narrows this by the source's relay-paired second view (a caller that withheld its ACK too charges the same withholding at two vantages) |
| `unexpected-prack` | a PRACK this platform sends to the reliable provisional the anchor step EMITS (RFC 3262 §4). The source never PRACKed it, so no step states one, and this platform's own answer arrives where nothing expects it |
| `unexpected-cancel` | a CANCEL this platform sends while the INVITE transaction is still in flight (RFC 3261 §9.1), where the source sent its CANCEL only after that transaction had taken — and ACKed — the final the anchor step EMITS. The capture places its CANCEL BEHIND that final, so the document's CANCEL step sits behind it too and this platform's arrives ahead of it, on a leg where nothing yet expects one. The anchor is the final rather than the CANCEL because a declaration's anchor must be a `send`: the transaction end is what the divergence turns on, and the emission is what supplies the dialog |

**A relay-paired second view is not a coordinate.** Where the same call's
UPSTREAM leg carries the very hit charged on the leg this platform derived from
it, the second is the first seen through the relay and billed to the wrong
party. It is dropped before the coverage ledger, so it neither declares nor
keeps a refusal standing, and ONE violation remains against the party that
committed it. The joiner is the CALL — the two legs usually share one peer
socket — so it is the deployment's Call-ID derivation that tells them apart.

**Derived, never hand-guessed.** A declaration is read off the census hit whose
detector §11.1 names, and never off the shape of the flow alone. A capture that
merely STOPPED before the answer looks identical from inside the document, and
only the detector's own window and liveness gates tell a withheld answer from a
truncated recording.

**Which side of the vantage decides whether there is anything to declare.** The
rule charges the endpoint that took the message and owed the answer; what it
ESTABLISHES is that no answer names that transaction anywhere on that leg,
relaying hops included. Where the scripted peer RECEIVES the 2xx or the
provisional, the withheld answer is the peer's own and replays faithfully: the
peer has no answering step, so it emits none, and nothing unexpected arrives.
Where the scripted peer SENDS it, the answer is owed by whatever sits on the
other side of the vantage, and on a replay that is this platform.

Lint holds the placement. `ref/must-fail-step-unknown` (the anchor names no step
of this flow), `must-fail/duplicate` (one failure declared twice on one anchor:
a run produces it once), `must-fail/anchor-not-a-2xx-send` (`unexpected-ack`
anchored on anything but a step that SENDS a 2xx to INVITE),
`must-fail/anchor-already-acked` (the flow states an ACK on that leg behind the
2xx, so the platform's ACK is the run being satisfied rather than failing), and
their PRACK twins `must-fail/anchor-not-a-reliable-provisional-send` (the
anchor must SEND a provisional to INVITE that states `Require: 100rel` or an
`RSeq` among its frozen headers) and `must-fail/anchor-already-pracked`.

**The derivation decides before the refusal** (amended 2026-08-25). Where the
census charges the capture's own system under test, the case is GENERATED as a
negative one wherever the derivation anchors a declaration for every charged
hit; the census refusal (`sut-violates:<rule>`) is DEFERRED and stands only on
the residue no declaration can anchor — a hit no step of the case carries,
because the scripted peer RECEIVES that message or the flow already states its
answer. Each refusal defers per RULE: one rule's anchored declaration never
withdraws another's refusal on the same coordinates. The only refusal decided
before assembly is the completeness gate (`source-call-incomplete`), and the two
decided AFTER it are read off the document rather than the capture: a vantage
holding only half of an INVITE's three-way handshake lost the other half to the
trace, whether that half is the final a leg ACKs without
(`source-final-not-captured`) or the ACK owed to a 2xx answering the SUT's own
INVITE (`source-ack-not-captured`, charged only where another leg holds the ACK
the B2BUA relays — an ACK is owed whatever a BYE did to the dialog, RFC 5407 §2
carving it out of the Mortal state's bar on new requests). A third is read there
too: an `18x` the caller is gated on that no called leg of the document ever
sources (`source-answer-not-captured`) — an 18x is relayed and never minted, so
the answer came off a leg the cut does not hold. Either way the case is not
written.

One boundary this section does NOT cross. The inversion is the VERDICT's: a run
reports the declared failure against what it observed, and no gate, no lint rule
and no recording is softened for it, so every recording stays a faithful account
of what happened.

**What a run does with a declaration.** Nothing special, and that is the point.
The gate refuses the declared datagram exactly as it refuses any other — same
failure, same site, same verbatim recording — and the run then goes on or ends
its script by §14's rule, which reads the same on every document. The declared
ACK is a REQUEST, so it ends nothing: the run walks past it and the scripted
teardown behind it still runs. Where something else on the same replay does end
the script, the tail past it is abandoned and the call is closed generically
(§14), which is also how a declaration can arrive with no expect armed to refuse
it.

The verdict decides last. Every declaration OBSERVED, nothing structural left
over, and the run reports **`ok-negative`** — spelled apart from `ok` so no
dashboard reads a case that passed BY FAILING as a case that passed. A
declaration is observed either way it can be: by the failure the gate raised,
listed under `must_fail` as `observed` — carrying the gate's own words verbatim,
the way a downgraded check lands under `informative` (§9.1) — or, where it
arrived after the script had ended and no expect was armed to refuse it, by the
RECORDING, listed as `recorded`. The carried wire divergences are listed the same
way, under `tolerated`. A declaration the run did not produce at all becomes a
failure of its own, `declared-failure-not-produced` — and a run with one carries
nothing, because a run that failed for another reason states its whole evidence
in `failures`.

The CDR expectation is asserted unchanged: a negative case's call is billed like
any other, and only what is IN the record is beyond a negative case's subject.

**Matched against the recording, never against a failure's wording.**
`unexpected-ack` is satisfied by an ACK on the ANCHOR's own leg that carries the
Call-ID, the INVITE CSeq number and the To-tag of the 2xx that step emitted, and
that no flow step claimed; `unexpected-prack` by a PRACK on that leg, in that
Call-ID, whose `RAck` names the anchor provisional's RSeq and the INVITE's CSeq
(RFC 3262 §7.2). An unexpected datagram of another method, in another dialog,
or on another leg satisfies nothing, however loudly it failed.

## 12. media — RESERVED

An open object, deployment-extensible. Nothing reads it. It exists so the media
vocabulary (issue 19: loadgen adoption) lands additively rather than as a
version bump. Distinct from `legs[].media`, which is the per-leg RTP source
token and is not reserved.

## 13. Validation

Three layers, and each catches what the one before it cannot.

- **Formatter** (§2.1): `pivot-schema fmt --check` in CI, `--write` to
  normalise. It parses through the structs, so it also proves conformance.
- **Schema**: `pivot-schema schema pivot` is the exported JSON Schema; the TS
  mirror is checked against it by `@sip/contracts`'s own test suite.
- **Lint**: `pivot-schema lint <file>` runs the semantic rules a schema cannot
  state. Non-zero on any error-severity finding.

Lint's rule groups:

| group | what it holds |
|---|---|
| `id/*` | every id unique, non-empty, dot-free; an identity name and its dial forms also free of the characters the number accessor reserves, and a bare position name refused once the document declares several calls |
| `ref/*` | every id, position and anchor a document names resolves inside it, an `rfc_violations` anchor and emitter included |
| `order/*` | every anchor and every `after` points backwards, and never into an `alt` branch it is not part of (§6.5) |
| `attempt/*`, `call/*` | `(branch, position)` unique, non-terminal attempts carry a cause, `no_answer_ms` inside the armable band, a `joined_by` leg joins at an unconditional step of its own call that precedes the leg's first message |
| `claim/*`, `lanes/*` | a lane verdict matches what the document asks the lane to do |
| `auto/*`, `check/*`, `optional/*`, `delay/*` | what a step may state given its `op` and its `auto` |
| `body/*` | `compare` rides an expect's resource body and no send's; `sdp` rides an `application/sdp` body (§8.3) |
| `alt/*` | discriminability: two branches minimum, unique names, none empty, none opening on a `send` or an `optional`, no first message two branches both match |
| `unordered/*`, `inject/*` | an order-free group holds two or more expects; an injection names an action |
| `background/*` | a settle-time counter states a bound, and the bounds agree |
| `deviation/*` | a defined `kind` carries the payload it needs (§11) |
| `must-fail/*` | a declared failure is stated once per anchor and lands on the shape its own member predicts (§11.2) |
| `cause/*` | a `cause` cites a dialog-creating final or an actual closer, never an in-dialog final (§4.1) |
| `in-dialog/*` | `in_dialog` is marked on EVERY step after the leg's dialog-creating final and on none at or before it, and `confirms_dialog` on the one ACK that answers such a final and on nothing else (§6.1) |
| `lane/*` (scoping) | a check class is stated beside an `origin_lane` there is something to compare it against (§9.1) |
| `accessor/*` | every `${…}` names something that exists, has already run, and is not inside a branch this reference is not part of; a `${num:…}` names a registered identity in a form it declares |
| `postconditions/*` | a CDR expectation is stated, or its absence is reasoned |
| `annotations/*` | a captured document accounts for the detector roster it names: every rostered detector states exactly one outcome (`detector-roster-incomplete`), and no outcome names a detector the roster omits (`detector-outcome-unrostered`) (§13.2) |
| `subset/*`, `capture/*` | the generator subset gate (§13.1) |

CI never diffs pcaps. It runs the scenario's own assertions, and the post-run
confrontation runs as its own pass over the recording.

### 13.1 The generator-subset gate

**A captured document may carry nothing a capture cannot justify.** A packet
trace shows what DID happen once; it never shows that an absence was tolerable,
that two orders were both acceptable, or that a value should be read from a
dialog at run time. A generator emitting `optional` or an accessor has inferred
something, and inference belongs to a human whose reasoning is reviewable.

Refused on `origin: capture`: more than one `calls[]` entry — `calls[]`
plurality is the authored construct that makes a call-limiter test expressible,
and a pcap yields one document per call — plus `background`, `alt`, `unordered`,
`inject`, `optional`, `after`, inline `checks`, postcondition checks, any
accessor —
`${num:…}` included, since substituting a number into a header the capture
froze is an inference — and a `cseq-override` relative to a run-time value.

The identity REGISTRY is not on that list, and deliberately so: a capture
observed the numbers it names, and the generator synthesizes one name per
position (§8.5). What a capture may not do is read one back into a header.

Neither is anything §9.1, §11.1 or §11.2 adds. `case.origin_lane` states where
the packets came from, a check class states whose vocabulary a stored value is,
an `rfc_violations` entry names a rule a detector decided off those same
packets, and a `must_fail` names what THIS lane does about one of those rules.
All four are readings OF the capture, not inferences beyond it: the last pairs a
detector's hit with the same lane knowledge §13.2 applies, and it is the one
construct by which a captured document may state something the capture does not
hold. A generated document states its origin lane; the rest is emitted once a
detector exists to justify it.

The gate runs the other way too. Required on `origin: capture`: `case.source`,
`timing.capture_span_ms`, and `observed` on every step.

### 13.2 Lane adaptation the generator applies

**A generated document states what the REPLAY will see, not only what the
capture showed** — but only where a difference between the source platform and
this one is a RULE rather than a judgement. §9.1 handles the differences that
are vocabulary (a fact stays, named, and the run decides what it costs). This
section handles the differences that are BEHAVIOUR, where leaving the capture
untransformed would encode a ladder no lane can run.

This is generator behaviour. It adds no field: what the generator changed rides
`case.annotations.flags`, whose `kind` is an open token.

**The ACK to a 2xx is no adaptation.** The source platform relays an ACK to a
2xx end to end — the b-leg ACK goes out when the a-leg ACK arrives — and so
does this one (§6.3): §13.2.2.4 gives the UAC core one ACK per 2xx and
re-passes THAT ACK for every copy, so the captured causality is the causality
the replay reproduces and there is nothing here to transform. The two things
that could have been adapted are stated where they belong: a `retransmits`
count on an auto ACK is drawn from the wire as the band the relayed ACK draws
(§6.3), and an ACK to a NON-2xx final is a transaction-layer ACK (RFC 3261
§17.1.1.3), hop-by-hop on every platform.

**The unreliable-provisional deficit.** A 1xx above 100 carrying no `RSeq`
rides no retransmission timer, so it does not repeat at all (§6.9): each
emission is its own message, and a relaying B2BUA passes each one on. Where the
captured platform dropped one on its own account the legs are not one for one,
and a document that scripts more emissions than it expects arrivals gates the
SUT on the next message while a datagram it was right to send goes unmatched.
So the generator derives the missing arrival, copying a captured one with its
`observed` coordinate — the SUT emits that message again, so it is what both
datagrams are compared against, and the second relay keeps its header
comparison. §6.9 states the three bounds and the mode that puts a case outside
them.

*Joined 2026-08-27 (issue 116) by the deficit above.* This slot held the
unreliable-provisional EXPANSION alone: a carve-out keeping a repeat of a class
that rides no ladder as steps rather than a count. It still holds — such a count
names a ladder no lane can run whoever wrote the document. What changed is the
population: the extractor states no unreliable-provisional repeat now, so the
flag rides a repeated **100 Trying**, or a document some other producer marked.

| flag | what it says |
|---|---|
| `unreliable-provisional-repeat-expanded` | these repeats of a class riding no ladder were kept as their own steps rather than a count, naming each by leg and captured message. On this extractor's output that class is the 100 Trying; an unreliable-provisional repeat reaches it only from another producer (§6.9) |
| `ack-count-drawn-from-final` | these ACK expectations carry the count they draw, naming each with the final it read, the number it composed against that final, and what the capture held (§6.3) |
| `provisional-expect-surplus-tolerated` | these caller-facing provisional expectations were stamped `optional`: the leg holds more relayed provisionals than peer emissions anchoring them, naming each with its leg, status and run (§6.9). The subset gate accepts no `optional` in a captured document without it |
| `relayed-provisional-expect-derived` | these caller-facing provisional expectations were derived from the emission that causes them: the leg holds fewer relayed provisionals than peer emissions, and each derived step copies a captured arrival, coordinate included, naming its leg, status, the emission it relays and the arrival it copies (§6.9) |
| `far-side-reinvite-derived` | these in-dialog INVITE exchanges the capture holds on one leg only were transcribed onto the far leg, whose record ends at the 2xx its peer sent: the relaying platform has the far leg on the other end, and each derived step — the INVITE, the 2xx, the ACK, each in the op that mirrors the near leg's — copies the near-leg message it mirrors, coordinate included, naming the leg, the step its record ends at, and every pair with its op (§6.9). The subset gate accepts no second step on one coordinate of those three shapes without it |
| `far-side-reinvite-not-derived` | these in-dialog INVITE exchanges onto a leg whose record ends at its 2xx were NOT transcribed — the platform answered the near peer's with a refusal other than 491 glare, which is its own; the near peer refused the far party's, a refusal relayed like a 2xx whose hop-by-hop ACK (§17.1.1.3) has no captured coordinate, or answered it 491, the near half of a crossing pair (§14.1) stated whole or not at all; or the far party's offer or answer is held by shape only — and the far leg scripts nothing for the relayed INVITE (§6.9) |

A count on an `expect` of a class something else DRAWS — the 100 Trying an
INVITE ladder pulls, one per copy (§17.2.1) — asks no lane to invent an
interval, and lint accepts it. The carve-out above is `send`-blind and takes the
100's repeats as steps regardless; the two readings do not conflict, because a
step per emission states everything a count would and the interval besides.

Every adaptation is reversible at regeneration: the synthesis pass still builds
the capture unchanged and each adaptation runs against it, so removing one
restores the captured ladder exactly.

**The detector roster.** A generated document also states what it LOOKED FOR.
§2.2 reads absence as "none" and nothing else, so a document carrying no
`relay18x` is indistinguishable between "transparent, decided" and "announcement
mode, never looked for", and a `family` whose rule has no arm for the shape in
front of it asserts a wrong answer where it owes an unknown. So a captured
document names the detectors this deployment runs and accounts for every one of
them, on the same `case.annotations.flags`:

| flag | what it says |
|---|---|
| `detector-roster` | the roster this document accounts for, comma-separated. This deployment's is `relay18x, prack, refer, reroute, fork, mrf` |
| `detected:<detector>` | the detector fired. `detail` quotes the signals verbatim, the way §4.2 `evidence` does |
| `detected-none:<detector>` | the detector ran over this capture and asserts the shape is absent |
| `detection-unavailable:<detector>` | the vantage lacks the messages the detector reads, so it decided nothing |

**`detection-unavailable` is a decision, not a failure.** "The shape is absent"
and "the messages that would settle it are not at this vantage" are different
statements, and a roster that collapsed them would be the silence it exists to
remove.

Every rostered detector states exactly one of the three on every generated
document, so the flag list is a complete account of what the extractor
understood; lint holds the document to the roster it named (`annotations/*`, §13)
so a new detector cannot ship silent. The roster is still an annotation: the
interpreter never reads it (§14).

## 14. Interpreter contract

The interpreter is deliberately dumb, with teeth.

**The replay tool never infers what to do.** A generated document carries
instructions explicit enough to drive the run end to end, the fake routing
decisions included: what the platform must be told to do lives in the document
and in the run configuration the driver compiled from it (§4.3), never in a
rule an interpreter derives from the shape of a flow. Where the document does
not say, the answer is a refusal that names what is missing — never a guess
that produces a green run for the wrong reason.

Its whole job:

1. **Bind** each endpoint per its `side` and `binding`.
2. **Sequence** the flow. Same-leg order is list order — save for a relay
   standing behind a send, which arms beside it (§6.7b) — cross-leg and
   cross-call order is `after`; a captured chain barrier comes from
   `attempts[].leg` plus `position`.
3. **`send`**: emit exactly what `msg` states, plus tier-1 regeneration, plus
   any run-config injected headers — the run's own, and on a call's DIAL that
   call's own directive (§4.3) — plus any accessor substitution: a `${num:…}`
   resolving through the identity binding the driver compiled (§4.3), never
   through a number in the document. Nothing else. The interpreter never judges
   one message a repeat of another, in either direction: it does not collapse,
   and it expands a count only for a class RFC 3261/3262 paces or one item 7
   draws.
4. **`expect`**: gate on op, leg alignment, discriminator and `within_ms`, whose
   budget opens at the step's own dwell (§6.8). Then apply `check`, then any
   inline `checks`. An `optional` expect is RELEASED — never failed — when a
   later step on its leg matches first, or when its own budget expires (§6.5).
5. **Lane scoping** (§9.1): evaluate every check, and record a CLASSIFIED one as
   informative instead of gating when the run's lane is not `case.origin_lane`,
   unless the run configuration states that class outright. One comparison, no
   vocabulary, no third status.
6. **`rfc_violations`** (§11.1): list every entry in the verdict. A scripted
   peer's gates nothing; the system under test's gates.
7. **`auto` steps**: the stack COMPOSES them — R-URI, Route, Via and CSeq off
   the transaction that obliged the message — and the step's STORED CONTENT
   rides them like any other step's: the frozen headers on every class, plus the
   body on the three that carry one (§6.3). Record the emission against the
   step, honour the step's `delay`, scope it to the transaction LEG STATE names.
   Never read an auto step's `cseq` — neither as a CSeq to emit nor as a key to
   resolve a transaction by. An auto ACK step's
   `retransmits` is DRAWN, not paced (§6.3): one copy per repeat of the final
   that transaction drew, the copies owed during a hold released with the ACK. A count on any other unpaced class
   — a sent unreliable provisional — is REFUSED before the wire, and lint
   refuses the document first (`retransmits/unpaced-provisional`).
8. **`background`**: answer per policy, record, and never move the flow cursor
   — but yield to a frontier `expect` whose budget is open and whose
   discriminator the arrival satisfies (§5.1).
9. **`alt`**: commit on the first discriminating message; never backtrack.
10. **`inject`**: hand the action token to the lane's injector. Never execute one.
11. **Dwell**: wait `delay.ms` from `delay.from` on a `send`. A virtual-clock
    lane may compress only where `compressible` is true. Never re-derive
    compressibility. On an `expect` that declares `timer_linked`, MEASURE the
    dwell instead — the system's timer ran it — and report declared against
    observed inside the run's stated window (§9.2).
12. **Record, always.** A verbatim per-leg recording of every message, in wire
    order, with arrival time, into the run bundle — in every mode, on every
    lane, whether or not anything asserted. A recorded datagram is BYTES
    (ADR-0035): the line writes them in exactly one of the extractor's three
    arms, chosen by the bytes alone — `raw` when the whole datagram is UTF-8,
    `head` + `body_b64` when only the body is not, `raw_b64` when not even
    the head is — beside the body's `body` layout (media type, byte length,
    MIME parts located by offset), so a reader finds a part without splitting
    on a boundary. One decoder reads captures and recordings alike; text is a
    rendering of the bytes, never the stored form.
13. **Settle** (§10), then evaluate `postconditions`.

**A failure does not end a run; being unable to GO ON does.** A run records every
failure it finds and keeps following its flow, because the next message is still
composable: a datagram nothing expected, one no armed expect matched while the
message that expect waits for can still arrive. What ends the SCRIPT is a failure
that leaves the run nothing to compose, and there are two:

- a required `expect` whose budget ran out — the message is MISSING;
- an arrival that leaves an armed expect UNSATISFIABLE — a FINAL response on the
  very transaction the expect is gated on, carrying a status it cannot match. RFC
  3261 §17.1 ends a client transaction at its final, so no response of another
  status rides it again. A provisional ends nothing, a request ends nothing, and
  a final for another transaction ends nothing. Where several expects are armed on
  one leg — `alt` branches, an `unordered` group — the arrival must contradict
  EVERY required one; an `optional` expect is released rather than failed (§6.5)
  and never blocks.

**This rule is polarity-free.** A `must_fail` declaration (§11.2) changes nothing
about WHEN a script ends — only what the verdict makes of what was recorded. A
positive run that cannot go on ends its script, closes its call and settles
exactly like a negative one, and still fails.

**The script ends; the call is closed generically.** Every scripted endpoint then
ends what it HOLDS, by the RFC's own rules and with no per-document scripting: it
answers a request it took and never answered (a BYE with 200, a CANCEL with 200,
an INVITE it will not answer with 480 — 487 where a CANCEL asked), acknowledges a
final it took (§13.2.2.4, §17.1.1.3), closes a dialog it OPENED with a BYE (§15),
and CANCELs an INVITE it sent that has a provisional and no final (§9.1). A leg
that ANSWERED its dialog never starts the teardown: the far side closes such a
dialog and this end answers what arrives, so a platform that tears nothing down
leaves the call up and §10 says so. The obligation is read off the leg's own
RECORDING and the message is composed by the leg's own stack, so the close puts
ordinary compliant SIP on the wire. It is bounded by `timing.settle_budget_ms`,
and a leg that still holds something open keeps the run from settling exactly
like a scripted dialog that is not terminal.

The run then settles and evaluates its postconditions like any other case. The
steps the script never ran do NOT raise `flow-incomplete`: they were abandoned by
this rule, and the verdict states that rather than hiding it — `completed_steps`
stays the truth about what ran, and an `abandoned` section names the leg and step
the script stopped at, the nodes it never ran, and every act the close emitted.

What the interpreter never does: read `calls` beyond leg sequencing and the id
and caller leg a per-call lane directive is placed by (`joined_by` included: a join explains the chain to a reviewer and a driver, and sequences
nothing the flow does not already state), read
`case.annotations` or `case.requires`, read `observed`, choose a matching
strictness, infer that a message was minted rather than relayed, re-split a
multipart body, or hold one line of callflow-specific logic.

**Compile once, run many.** A v3 document compiles to an immutable per-call
plan; each call instance is that plan plus identity substitution (numbers,
Call-IDs, ports), with no per-call parsing and no per-call document allocation.

## 15. Marked fields

Everything in this document is frozen except the following.

| item | status | reason |
|---|---|---|
| `rfc_violations` vocabulary | **MINIMAL** | §11.1. One member, `no-200-after-cancel`, and no `allowed` flag. The vocabulary and its mechanics grow from the per-endpoint violation census, not before it |
| `must_fail` vocabulary | **MINIMAL** | §11.2. Two members, `unexpected-ack` and `unexpected-prack`. It grows with the prediction that decides it, as `rfc_violations` grows with the detector |
| `inject` execution semantics | **DESIGN ONLY** | §6.6 freezes the shape and the injector-interface split. No interpreter executes an action in this program |
| `media` | **RESERVED** | §12. Open object, deployment-extensible; the vocabulary lands in issue 19 |
| match-less retry chains | **UNEXERCISED** (F3) | a retry chain ordered by a match-less rule is absent from the corpus. `calls[].attempts` expresses it; nothing has produced one |
| body-registry deployment overlay | **NOT MODELLED** (G6) | §8.2. A deployment handler that REWRITES a body has no field on a `body`/`part` to name itself with. No corpus case exercises it |
