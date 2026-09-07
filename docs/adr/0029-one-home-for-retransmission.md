# One home for retransmission: a shared schedule, an opaque retained datagram, and obligations the framework discharges

**Status:** accepted (2026-09-05)

## Context

Five sites implement the same doubling retransmission rule, in two units and
two crates:

| site | class | state carried |
|---|---|---|
| `sip-txn/src/layer/client.rs:417-432` | Timer A / E | `interval_ms` + `elapsed_ms` + `max_ms` (u64) |
| `sip-txn/src/layer/client.rs:311` | CANCEL Timer E (ADR-0028 X4) | `HeldCancel` interval + elapsed |
| `sip-txn/src/layer/server.rs:32-61` | Timer G | `interval_ms` |
| `b2bua/src/rules/actions/respond.rs:257-276` | §13.3.1.4 2xx | `ack_retransmit_interval_ms` (i64, replicated) |
| `b2bua/src/rules/actions/reliable_ladder.rs:97-158` | RFC 3262 §3 | `interval_ms` (i64, replicated) |

A sixth, `pivot-interpreter/src/retransmit.rs`, re-derives the same schedule as
the audit model that confronts the SUT, and the TypeScript cut walks T1/T2 a
seventh time in `ts/pipeline/src/drawn-ack.ts`.

Two structural defects follow from the split.

**A repeat can be re-composed rather than repeated.** RFC 3261 §13.3.1.4 and
RFC 3262 §3 both make a retransmission *the same response*, and both sites say
so in a doc comment — but the retained bytes reach the socket through
`OutboundBody::Response`, so `emit_outbound` (`router/interpret.rs:224`)
re-parses and re-serializes them. Every field the parser normalises or the
serializer re-orders is a divergence the type system permits. This class of
bug has been fixed three times from the wire (`0be93ff`, `0fd2c3a`, `dee5e36`)
rather than removed.

**A B2B rule manages ladder timers by hand.** `relay-ack`
(`rules/defaults/core_rules.rs:895-931`) cancels two timer pairs, clears the
retained datagram, and compares a CSeq against `pending_reinvite_2xx` to pick
between them; a downstream MRF overlay arms the same pair itself. The
transaction layer asks
nothing of its consumers for the ladders it owns — the asymmetry is not the
distinction between transaction-level and dialog-level retransmission, it is
the absence of a seam for the latter.

## Decision X1 — a leaf crate, `sip-retransmit`, owns the schedule

`crates/sip-retransmit` holds the RFC §17 timer constants (moved from
`sip_txn::timers`, which re-exports them) and a `Schedule` in three forms:

- `Schedule::rfc(Class)` — T1, doubling, with the cap and give-up bound of the
  class: `InviteClient`, `NonInviteClient`, `NonInviteProceeding`,
  `CancelClient`, `InviteServerFinal`, `Final2xx`, `ReliableProvisional`.
  `NonInviteProceeding` is §17.1.2.2's flat-T2 re-arm as a class of its own
  rather than a clamp at the call site, so a ladder that changes pace mid-flight
  changes it through `Ladder::retarget` and keeps an honest elapsed total.
- `Schedule::exact(intervals, give_up)` — one stated interval per rung, in
  order: the pacing a capture measured rather than the pacing an RFC prescribes.
- `Schedule::once()` — emitted once, no rung. Distinct from `rfc`, which the
  audit model conflated by treating an empty interval list as "pace by the RFC".

A schedule answers `interval(rung)`, `elapsed_at(rung)` and `give_up_after()`;
`with_give_up` replaces the bound with the owner's own, which may be tighter
than the class's (a deployment may configure an INVITE bound below Timer B, and
RFC 3261 lets the tighter deadline own the give-up). The crate has no clock, no
timers and no I/O.

**`exact` and `once` sit behind the `authored` cargo feature**, enabled by
`pivot-interpreter` and the harnesses and never by `b2bua` or `sip-txn`, so a
harness simulating a badly behaved endpoint and a replay of a capture whose
platform paced its own ladder keep the form they need. Cargo unifies features
across a workspace build, so the guarantee is per-package: CI asserts it with
`cargo build -p sip-txn -p b2bua -p call -p sip-proxy`, where a non-RFC ladder
is a compile error rather than a review comment.

It is a pure leaf on the ADR-0002 model, like `sip-clock`: `sip-txn`, `call`,
`b2bua` and `pivot-interpreter` depend on it and nothing depends back.

**Amendment — the initial INVITE's first-response bound.** A deployment may
tighten the bound an *initial* INVITE (no To-tag) waits in `Calling` for a
response of any kind below Timer B:
`TransactionConfig::invite_first_response_timeout_ms` (default Timer B),
surfaced as `B2BUA_INVITE_FIRST_RESPONSE_TIMEOUT_SEC` (default 32, range
`2..=32`). It is a deliberate RFC 3261 §17.1.1.2 deviation, recorded as
telephony policy: a hop that draws nothing — not even a `100 Trying` — is
dead, and a caller cannot be left in silence for 64·T1 before the reroute to
an alternate hop. The Timer A ladder is armed under the same bound, so no rung
lands past the give-up and the bound buys a rung count — 2 s → 2 re-sends
(0.5/1.5 s), 5 s → 3, 10 s → 4, 32 s → 6; the 2 s floor guarantees a lossy
path more than one chance, and the ceiling is the RFC value itself. The
expiry keeps `TimeoutKind::Response` ("nothing answered"), and the
`call_failure` consult carries it as `timeout_kind: "response" | "transaction"`
beside an unchanged `origin`. An in-dialog INVITE and every non-INVITE keep
64·T1 (Timer B / Timer F), and the first provisional still swaps in the long
`invite_initial_timeout_ms` bound. The proxy's `cancel_lru` TTLs do not
follow: they are ceilings derived from the long bound.

## Decision X2 — the only ladder state is a rung index

A **rung** is one step of a ladder: rung 0 is the original send, rung 1 the
first re-send, and so on. Interval, elapsed time and "past the bound" all
derive from the class, so a rung index is the whole of a ladder's state, and
`Ladder::at_rung` rebuilds one whole from it. This replaces three different
field sets and keeps the property the §3 ladder reasoned for in a comment: no
epoch anchor rides in a replicated body, so a takeover resumes the ladder
exactly where it stood.

The `Ladder` cursor carries the elapsed total beside the rung, because a
retargeted ladder's elapsed time is no longer a function of its rung alone.
Only the non-INVITE client transaction retargets, and its state is node-local:
every replicated ladder stores a rung index and nothing else.

## Decision X3 — one opaque `RetainedEmission`, carried as a datagram

`AnsweredInvite2xx`, `PendingReinvite2xx`, `ReliableProvisionalEmission` and
`EmittedAck` merge into one replicated `RetainedEmission { datagram, dest,
repeat }`, where `repeat` is `Paced { class, rung }` or `OnTrigger` (the
§13.2.2.4 re-ACK of a repeated 2xx, and the server transaction's replay of a
cached final).

The datagram is opaque: its only outward operation yields the bytes and the
destination. `OutboundBody::Datagram` carries it through `emit_outbound` with no
parse and no serialize, and a raw response bypass is now only ever a retained
datagram. That last is enforced at the one seam rather than in the types —
`emit_outbound` routes every `Response` body through its server transaction
whatever the mode, with a `debug_assert` on a raw one — because body and mode
are separate fields and coupling them would reshape fifty match arms in
unrelated files.

**The retained bytes are the first copy's bytes by construction, not by two
renderers agreeing.** A typed message carries its datagram — `image()`, parsed
from the wire or rendered once at freeze, never edited in place (ADR-0025) —
and the transaction layer's `send_response` puts that image on the wire
verbatim; it renders nothing. The retaining rule keeps the same image, the
recorder stores it, and a rung sends it raw. One rendering, several holders:
the first copy, every rung and the trace are the same bytes. The only
constructor that built a message without an image (`hydrate_response`) is
gone, and `hydrate_request` renders one, so no typed message reaches the layer
imageless. A second render at the socket would have to be written back into
`sip-txn` against the doc contract on `send_response`, and
`sip-txn/tests/response_leaves_as_its_image.rs` pins the wire to the image.

`rfc_rules` gains the wire twin of the invariant: a rung must be byte-identical
to the emission it repeats, checked in every harness `finish()` and across the
corpus census. The one row the comparison leaves out of each copy is a
`Record-Route` whose every hop names the emitter itself — its own per-forward
stamp (RFC 3261 §16.6), which a taker that absorbs the copy as a
retransmission under the §16.11-fixed branch never reads twice.

## Decision X4 — the framework discharges an obligation; a rule owns only the give-up

A transaction ladder stays invisible to the rules, as today. A dialog-level
ladder becomes an **obligation** the framework tracks:

- `AckOf2xx { leg, dialog_tag, cseq }` — discharged by an ACK of that CSeq on
  that dialog. One key covers the initial INVITE and the re-INVITE, so the twin
  timer pair and `relay-ack`'s CSeq comparison both disappear.
- `PrackOf { a_tag, a_rseq }` — discharged by a PRACK whose RAck names that
  RSeq, matched on the whole RAck so this and `relay-prack`'s 481 cannot
  disagree.

A `Non2xxAck` obligation is deliberately absent: `sip-txn` owns Timer G/H, does
not depend on `call`, and nothing would construct one.

The match is deterministic RFC, so the engine performs it before the rules run
and cancels the ladder itself. The seam is `rule_chain_turn`, the one place an
in-dialog event becomes a rule turn with the whole `Call` in hand. A rule that answers with a 2xx says so once and
is done; it never pushes `CancelTimer` for a ladder, and `ClearAnsweredInvite2xx`
/ `ClearPendingReinvite2xx` are deleted from the SDK.

The one thing that reaches a rule is the give-up, as
`TimerType::RepeatGiveUp { obligation }` — a single event kind a service rule
may override per obligation. Existing CDR marker strings are unchanged.

## Decision X5 — the 2xx ladder is not configurable away, and neither is its give-up

`ack_timeout_sec <= 0` disabled the §13.3.1.4 ladder along with its give-up.
Not retransmitting an un-ACKed 2xx is a protocol violation, so the ladder always
runs to 64·T1 (Timer L). And RFC 3261 §13.3.1.4 has the UAS that "retransmits
the 2xx response for 64*T1 seconds without receiving an ACK" end the session
with a BYE: a 2xx that never draws its ACK means the peer is gone, and on a
B2BUA an answered-but-un-ACKed bridged call would otherwise leak until the
`GlobalDuration` cap (the a-leg INVITE server transaction went `Completed` on
the 2xx and is deleted silently at Timer H). So the teardown is unconditional
too, and `ack_timeout_sec` is its **deadline**, never a switch: `validate`
refuses a non-positive value the way it refuses one for
`invite_txn_timeout_sec`, and a config that bypasses validation falls back to
the 32 s default (`ack_timeout_ms`). The two bounds are distinct: the ladder's
is protocol — `min(ack_timeout_sec, Timer L)` (`Schedule::tightened_to`: a
deadline sooner than Timer L cuts the ladder short, since no rung is sent into a
torn-down call; one later than Timer L adds no rung) — and the give-up's is
policy, the configured deadline on either side of Timer L.

A service rule may re-author the give-up — its cause, its CDR, the order it
releases the legs — but not whether the session ends. The framework settles the
`AckOf2xx` give-up after the rules have run (`settle_give_up`): the retained 2xx
leaves the replicated body whatever they decided (its RFC 6026 *Accepted*
interval is over, so `reinvite-glare` stops answering 491), and a call still
Active is torn down with the CORE verdict. The `PrackOf` give-up keeps its own
RFC 3262 §3 policy — reject the pending re-INVITE, absorb, or tear down — and
is left to the rules.

The one consumer of the old behaviour is `failover-harness`
(`src/harness.rs:605-612`), whose token-for-token differential oracle could not
align the retransmits. Since X3 makes every rung byte-identical by
construction, the oracle folds byte-identical datagrams from one emitter into
one token — which makes the oracle itself an assertion of the X3 invariant.

## Consequences

- The audit model in `pivot-interpreter` adapts onto `sip-retransmit` for
  pacing and keeps `Closer`, `Repeats` and `DrawnAcks`, so the SUT and the
  oracle that confronts it cannot disagree about a schedule.
- ADR-0007 (transaction layer shape) and ADR-0010 (B2BUA rules shape) keep
  their decisions; this ADR moves the dialog-level retransmission obligation
  from the rule layer to the framework and gives both layers one schedule.
- ADR-0014's reactive-only takeover is unaffected: a rung index and an opaque
  datagram replicate exactly as the interval fields they replace did.
- The B2BUA data model loses four retained structs and three interval fields;
  the SDK loses two `RuleAction` variants and gains none.

## Pinned by

`sip-retransmit`'s table-driven schedule test (one row per class, plus the
authored forms and `tightened_to`), the existing
`sip-txn/tests/{fsm,cancel_retransmit,cancel_hold}` suite unchanged,
`sip-txn/tests/response_leaves_as_its_image.rs` (the wire is the image),
`b2bua-harness/tests/{unacked_2xx_retransmit_is_faithful,
unacked_2xx_reap,unacked_reinvite_2xx_reap,prack_reliable_ladder}.rs`
(`unacked_2xx_reap` includes the Timer L ceiling under a 60 s deadline),
`b2bua-harness/tests/unacked_2xx_reap.rs` also holds the X5 floor: a
non-positive `ack_timeout_sec` still ends the session at Timer L, and a service
rule that answers the give-up without terminating does not keep it up.
`b2bua/tests/rules.rs` (answering the caller leaves a b-leg's pending re-INVITE
2xx ladder alone; `settle_give_up` ends a session a service parked and leaves a
`PrackOf` give-up to the rules), `b2bua-sdk/src/config.rs` (a non-positive
deadline is refused and falls back to 32 s), `call/tests/model_helpers.rs` (the 2xx ladder never runs
past Timer L; `Scope::Provisionals` names no 2xx),
`failover-harness/tests/prack_takeover.rs`, and the new byte-identity rule in
the `rfc-rules` census.
