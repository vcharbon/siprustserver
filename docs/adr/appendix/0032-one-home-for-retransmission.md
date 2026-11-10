# Appendix: One home for retransmission: a shared schedule, an opaque retained datagram, and obligations the framework discharges

The decision part: [ADR-0032](../0032-one-home-for-retransmission.md).

## Context

The split X1 to X4 merge, with file references as of the decision: five sites implement the
same doubling retransmission rule, in two units and two crates.

| site | class | state carried |
|---|---|---|
| `sip-txn/src/layer/client.rs:417-432` | Timer A / E | `interval_ms` + `elapsed_ms` + `max_ms` (u64) |
| `sip-txn/src/layer/client.rs:311` | CANCEL Timer E (ADR-0058 X4) | `HeldCancel` interval + elapsed |
| `sip-txn/src/layer/server.rs:32-61` | Timer G | `interval_ms` |
| `b2bua/src/rules/actions/respond.rs:257-276` | §13.3.1.4 2xx | `ack_retransmit_interval_ms` (i64, replicated) |
| `b2bua/src/rules/actions/reliable_ladder.rs:97-158` | RFC 3262 §3 | `interval_ms` (i64, replicated) |

A sixth, `pivot-interpreter/src/retransmit.rs`, re-derives the same schedule as
the audit model that confronts the SUT, and the TypeScript cut walks T1/T2 a
seventh time in `ts/pipeline/src/drawn-ack.ts`.

**A repeat can be re-composed rather than repeated.** RFC 3261 §13.3.1.4 and
RFC 3262 §3 both make a retransmission *the same response*, and both sites say
so in a doc comment, but retained bytes that reach the socket through
`OutboundBody::Response` are re-parsed and re-serialized by `emit_outbound`
(`router/interpret.rs:224`). Every field the parser normalises or the serializer
re-orders is a divergence the type system permits.

**A B2B rule manages ladder timers by hand.** `relay-ack`
(`rules/defaults/core_rules.rs:895-931`) cancels two timer pairs, clears the
retained datagram, and compares a CSeq against `pending_reinvite_2xx` to pick
between them; a downstream MRF overlay arms the same pair itself. The
transaction layer asks
nothing of its consumers for the ladders it owns — the asymmetry is not the
distinction between transaction-level and dialog-level retransmission, it is
the absence of a seam for the latter.

## Decision X1: a leaf crate, `sip-retransmit`, owns the schedule

The classes of `Schedule::rfc(Class)` are `InviteClient`, `NonInviteClient`,
`NonInviteProceeding`, `CancelClient`, `InviteServerFinal`, `Final2xx` and
`ReliableProvisional`. `NonInviteProceeding` is §17.1.2.2's flat-T2 re-arm as a class of its
own rather than a clamp at the call site, so a ladder that changes pace mid-flight changes it
through `Ladder::retarget` and keeps an honest elapsed total. `Schedule::exact` paces as a
capture measured, not as an RFC prescribes. `Schedule::once()` is distinct from `rfc`: an
empty interval list never means "pace by the RFC".

`with_give_up` exists because a deployment may configure an INVITE bound below Timer B, and
RFC 3261 lets the tighter deadline own the give-up.

`exact` and `once` are enabled by `pivot-interpreter` and the harnesses, so a harness
simulating a badly behaved endpoint and a replay of a capture whose platform paced its own
ladder keep the form they need. Cargo unifies features across a workspace build, so the
guarantee is per-package: in the per-package CI build a non-RFC ladder is a compile error
rather than a review comment. `sip-retransmit` is a pure leaf like `sip-clock`.

## Decision X2: the only ladder state is a rung index

The rung index keeps the property the §3 ladder reasoned for in a comment: with no epoch
anchor in a replicated body, a takeover resumes the ladder from the last replicated write,
bounded by the deadline in the ledger. A rung rides only in the next write because "a counter
counts writes that change the call, not progress" (ADR-0014). A retargeted ladder's elapsed
time is not a function of its rung alone, hence the elapsed total in the cursor.

## Decision X3: one opaque `RetainedEmission`, carried as a datagram

`RetainedEmission` merges `AnsweredInvite2xx`, `PendingReinvite2xx`,
`ReliableProvisionalEmission` and `EmittedAck`. A raw response bypass is only ever a retained
datagram. The seam enforces that, not the types, because body and mode are separate fields
and coupling them would reshape fifty match arms in unrelated files.

**The retained bytes are the first copy's bytes by construction, not by two
renderers agreeing.** A typed message carries its datagram, `image()`, parsed
from the wire or rendered once at freeze, never edited in place (ADR-0025),
and the transaction layer's `send_response` puts that image on the wire
verbatim; it renders nothing. The retaining rule keeps the same image, the
recorder stores it, and a rung sends it raw. One rendering, several holders:
the first copy, every rung and the trace are the same bytes. No constructor
builds a message without an image (`hydrate_request` renders one), so no typed
message reaches the layer imageless. A second render at the socket would have
to be written back into `sip-txn` against the doc contract on `send_response`,
and `sip-txn/tests/it/response_leaves_as_its_image.rs` pins the wire to the image.

The `Record-Route` the byte comparison leaves out of each copy is the emitter's own
per-forward stamp (RFC 3261 §16.6), which a taker that absorbs the copy as a retransmission
under the §16.11-fixed branch never reads twice.

## Decision X4: the framework discharges an obligation; a rule owns only the give-up

One `AckOf2xx` key covers the initial INVITE and the re-INVITE, so no twin timer pair and no
CSeq comparison in `relay-ack` exist. `AwaitPrack` names the reliable provisional shown as
`rseq` in this stack's `tag` dialog, and shares its match with `prack_disposition`, so the
discharge and the PRACK's answer cannot disagree. A `Non2xxAck` obligation is deliberately
absent: `sip-txn` owns Timer G/H, does not depend on `call`, and nothing would construct one.

The match is deterministic RFC, which is why the engine performs it. `rule_chain_turn` is the
one place an in-dialog event becomes a rule turn with the whole `Call` in hand. A rule that
answers with a 2xx says so once and is done. Existing CDR marker strings are unchanged.

## Decision X5: the 2xx ladder is not configurable away, and neither is its give-up

Not retransmitting an un-ACKed 2xx is a protocol violation. RFC 3261 §13.3.1.4 has the UAS
that "retransmits the 2xx response for 64*T1 seconds without receiving an ACK" end the
session with a BYE: a 2xx that never draws its ACK means the peer is gone, and on a B2BUA an
answered-but-un-ACKed bridged call would otherwise leak until the `GlobalDuration` cap (the
a-leg INVITE server transaction went `Completed` on the 2xx and is deleted silently at Timer
H). `validate` refuses a non-positive `ack_timeout_sec` the way it refuses one for
`invite_txn_timeout_sec`. The two bounds are distinct. The ladder's is protocol: a deadline
sooner than Timer L cuts the ladder short, since no rung is sent into a torn-down call, and
one later than Timer L adds no rung. The give-up's is policy, the configured deadline on
either side of Timer L.

`settle_give_up` drops the retained 2xx whatever the rules decided: its RFC 6026 *Accepted*
interval is over, so `reinvite-glare` stops answering 491.

`AwaitPrack` arms no give-up. The provisional it repeats relays a responder's
own, whose PRACK is relayed end to end, so RFC 3262 §3's "reject the original
request with a 5xx" belongs to that responder: this stack's ladder stops at its
64·T1 bound, and the responder's reject reaches the PRACKing party as the
INVITE's final through the ordinary failure path. A second timer here would race
the responder's for the same bound and answer the INVITE in its place.

`failover-harness` (`src/harness.rs:605-612`) runs a token-for-token differential oracle
that cannot align retransmits on its own. Since X3 makes every rung byte-identical by
construction, the oracle folds byte-identical datagrams from one emitter into one token,
which makes the oracle itself an assertion of the X3 invariant.

## Decision X6: a reliable provisional is one record, and the PRACK this stack owes is the engine's

X4 makes the PRACK owed *to* this stack an obligation the framework discharges. X6 does the
same for the PRACK this stack owes its *responder*. With a PRACK emitted by each rule that
absorbs a reliable provisional, a sweep in each teardown path, and "a PRACK is owed" read from
gaps between two lists, a rule or a path that forgets leaks a provisional the responder
repeats to its give-up (RFC 3262 §4).

- **One record.** `Call.provisionals` holds one `Provisional` per reliable
  provisional received: `ProvisionalId { leg, responder_tag, invite_cseq, rseq }`
  (typed `CSeq`/`RSeq`), what the responder is owed (`ResponderAck`: `Pending`,
  `RelayedPrack`, `OwnPrack { branch }`, `DeferredToAck`, `Waived`), and, when
  it was shown, where and how (`Shown { tag, rseq, invite_cseq, ladder }` with
  `Ladder`: `Unsent`, `Repeating`, `Acknowledged`, `Ceased`). The two faces are
  separate facts: this stack may have PRACKed the responder and the party
  shown it may PRACK later.
- **Recorded by the engine before the rules.** A reliable provisional that
  repeats a recorded one (§3) or is out of `RSeq` order (§4) is dropped there and
  reaches no rule. A new one is recorded, opens its early dialog (RFC 3261
  §12.1.2) and notes the offer it carries. A rule only chooses to relay or absorb.
- **One PRACK step.** `settle_owed_pracks` PRACKs what is `Pending` and shown to
  no one live. It runs inside the primitives that end a dialog or an INVITE —
  the CANCEL and BYE builders, the re-INVITE CANCEL, confirm's retirement of the
  losing forks, the INVITE's final — so the PRACK leaves ahead of the CANCEL or
  BYE on its dialog, and once more after the rules. Never twice per `RSeq`, in
  §4 order per dialog. A PRACK owed on a dialog the leg lacks is a model
  defect; `Waived` is only for a dialog already ended (RFC 3261 §15.1.2). Where
  no party answered an offer the PRACK owes an answer to, it carries one
  rejecting every stream (RFC 3262 §5, RFC 3264 §6).
- **One answer to a received PRACK.** `prack_disposition` gives `Relay`,
  `Answer200` or `Refuse481`, in-call and from the released call's late-PRACK
  book alike: a PRACK naming nothing shown, or a provisional its party already
  acknowledged, draws 481 here (§3), never a relay.
- **Owed answers on the dialog.** `Dialog.owed_answer` (`InAck`, `InPrack`,
  `DeferredToAck`) is the one offer a dialog this stack dialled still owes an
  answer to (RFC 3264 §4), and the message that carries it.
- **Opt-in.** A provisional is shown reliably only to an INVITE that offered
  `100rel` (§3); otherwise it leaves unreliably and the engine PRACKs it. The
  live audit's `reliable-needs-client-opt-in` gates rather than advises.

Per-turn invariants hold the step to its contract: no `Pending` provisional
shown to no one at the end of a turn, none on a leg whose INVITE has its final
or is ending, and a shown ladder `Repeating` exactly while its `AwaitPrack`
rung is armed, with never a give-up.

## Consequences

- The audit model in `pivot-interpreter` keeps `Closer`, `Repeats` and `DrawnAcks`.
- An opaque datagram replicates with the write that retains it, and the rung index rides only
  in the next write that changes the call.

## Pinned by

`sip-retransmit`'s table-driven schedule test (one row per class, plus the
authored forms and `tightened_to`), the existing
`sip-txn/tests/it/{fsm,cancel_retransmit,cancel_hold}` suite unchanged,
`sip-txn/tests/it/response_leaves_as_its_image.rs` (the wire is the image),
`b2bua-harness/tests/it/{unacked_2xx_retransmit_is_faithful,
unacked_2xx_reap,unacked_reinvite_2xx_reap,prack_reliable_ladder}.rs`
(`unacked_2xx_reap` includes the Timer L ceiling under a 60 s deadline),
`b2bua-harness/tests/unacked_2xx_reap.rs` also holds the X5 floor: a
non-positive `ack_timeout_sec` still ends the session at Timer L, and a service
rule that answers the give-up without terminating does not keep it up.
`b2bua-core/tests/it/rules/` (answering the caller leaves a b-leg's pending re-INVITE
2xx ladder alone; `settle_give_up` ends a session a service parked), `b2bua-sdk/src/config.rs` (a non-positive
deadline is refused and falls back to 32 s), `call/tests/it/model_helpers.rs` (the 2xx ladder never runs
past Timer L; `Scope::Provisionals` names no 2xx),
`failover-harness/tests/it/{prack_takeover,prack_repeat_takeover}.rs`, and the
byte-identity rule in the `rfc-rules` census. X6:
`b2bua-harness/tests/it/{prack_disposition,reliable_1xx_out_of_order,
reliable_provisional_repeat_prack,prack_owed_at_final,prack_owed_at_cancel,
prack_rejects_an_unanswered_offer}.rs`, `b2bua/src/router/provisional_turn_tests.rs`,
the provisional invariants in `b2bua/src/rules/model_invariants_provisionals.rs`,
and the `prack-2xx-or-481` and `reliable-needs-client-opt-in` rules.
