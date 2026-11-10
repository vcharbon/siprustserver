# One home for retransmission: a shared schedule, an opaque retained datagram, and obligations the framework discharges

**Status:** accepted (2026-09-05)

The sites this decision merges, the rationale of each section, the long form of X6 and the
tests that pin it: [appendix](appendix/0032-one-home-for-retransmission.md).

## Context

The doubling retransmission rule had five implementations in two units and two crates,
re-derived again by the audit model and the TypeScript cut. Two defects follow from the split.
A repeat can be re-composed rather than repeated: the retained bytes reach the socket as a
response that is re-parsed and re-serialized, against RFC 3261 §13.3.1.4 and RFC 3262 §3,
which make a retransmission the same response. And a B2B rule manages ladder timers by hand,
because dialog-level retransmission has no seam. Detail:
[appendix, context](appendix/0032-one-home-for-retransmission.md#context).

## Decision X1: a leaf crate, `sip-retransmit`, owns the schedule

`crates/sip-retransmit` holds the RFC §17 timer constants (`sip_txn::timers` re-exports them)
and a `Schedule` in three forms:

- `Schedule::rfc(Class)`: T1, doubling, with the cap and give-up bound of the class.
- `Schedule::exact(intervals, give_up)`: one stated interval per rung, the pacing a capture
  measured.
- `Schedule::once()`: emitted once, no rung.

A schedule answers `interval(rung)`, `elapsed_at(rung)` and `give_up_after()`. `with_give_up`
replaces the bound with the owner's own, which may be tighter than the class's. The crate has
no clock, no timers and no I/O. `exact` and `once` sit behind the `authored` cargo feature,
never enabled by `b2bua` or `sip-txn`. CI asserts it with
`cargo build -p sip-txn -p b2bua -p call -p sip-proxy`. It is a pure leaf on the ADR-0002
model: `sip-txn`, `call`, `b2bua` and `pivot-interpreter` depend on it and nothing depends back.

The initial INVITE's first-response deadline is the TU's, beside an untouched Timer B
(ADR-0044). Detail: [appendix X1](appendix/0032-one-home-for-retransmission.md#decision-x1-a-leaf-crate-sip-retransmit-owns-the-schedule).

## Decision X2: the only ladder state is a rung index

A rung is one step of a ladder: rung 0 is the original send, rung 1 the first re-send. Interval,
elapsed time and "past the bound" derive from the class, so a rung index is the whole of a
replicated ladder's state, and `Ladder::at_rung` rebuilds one from it. No epoch anchor rides in
a replicated body. A rung is a quiet turn and rides only in the next write (ADR-0014). The
`Ladder` cursor carries the elapsed total beside the rung for the one ladder that retargets,
the non-INVITE client transaction, whose state is node-local. Detail:
[appendix X2](appendix/0032-one-home-for-retransmission.md#decision-x2-the-only-ladder-state-is-a-rung-index).

## Decision X3: one opaque `RetainedEmission`, carried as a datagram

One replicated `RetainedEmission { datagram, dest, repeat }` holds every retained emission.
`repeat` is `Paced { class, rung }` or `OnTrigger` (the §13.2.2.4 re-ACK of a repeated 2xx, and
the server transaction's replay of a cached final). The datagram is opaque: its one outward
operation yields the bytes and the destination. `OutboundBody::Datagram` carries it through
`emit_outbound` with no parse and no serialize. `emit_outbound` routes every `Response` body
through its server transaction, with a `debug_assert` on a raw one.

The retained bytes are the first copy's bytes by construction. A typed message carries its
`image()` (ADR-0025), `send_response` puts it on the wire verbatim, and the retaining rule, the
recorder and every rung hold the same image. `rfc_rules` checks that a rung is byte-identical
to the emission it repeats, in every harness `finish()` and across the corpus census, leaving
out only a `Record-Route` whose every hop names the emitter. Detail:
[appendix X3](appendix/0032-one-home-for-retransmission.md#decision-x3-one-opaque-retainedemission-carried-as-a-datagram).

## Decision X4: the framework discharges an obligation; a rule owns only the give-up

A transaction ladder stays invisible to the rules. A dialog-level ladder is an obligation the
framework tracks:

- `AckOf2xx { leg, dialog_tag, cseq }`, discharged by an ACK of that CSeq on that dialog. One
  key covers the initial INVITE and the re-INVITE.
- `AwaitPrack { tag, rseq }`, discharged by a PRACK whose RAck names it on all three §7.2
  tokens, the match `prack_disposition` answers by (X6).

There is no `Non2xxAck` obligation: `sip-txn` owns Timer G/H. The engine performs the match
before the rules run, at `rule_chain_turn`, and cancels the ladder itself. A rule never pushes
`CancelTimer` for a ladder. The one thing that reaches a rule is the give-up, as
`TimerType::RepeatGiveUp { obligation }`, which a service rule may override per obligation.
Only `AckOf2xx` arms one (X5). Detail: [appendix X4](appendix/0032-one-home-for-retransmission.md#decision-x4-the-framework-discharges-an-obligation-a-rule-owns-only-the-give-up).

## Decision X5: the 2xx ladder is not configurable away, and neither is its give-up

The §13.3.1.4 ladder always runs, bounded by `min(ack_timeout_sec, Timer L)`
(`Schedule::tightened_to`). Its give-up always ends the session with a BYE. `ack_timeout_sec`
is the give-up's deadline, never a switch: `validate` refuses a non-positive value, and a config
that bypasses validation falls back to the 32 s default (`ack_timeout_ms`). A service rule may
re-author the give-up (its cause, its CDR, the order it releases the legs) but not whether the
session ends: `settle_give_up` runs after the rules, drops the retained 2xx from the replicated
body, and tears down a call still Active with the CORE verdict. `AwaitPrack` arms no give-up:
the responder whose provisional it relays owns the RFC 3262 §3 5xx. The `failover-harness`
oracle folds byte-identical datagrams from one emitter into one token. Detail:
[appendix X5](appendix/0032-one-home-for-retransmission.md#decision-x5-the-2xx-ladder-is-not-configurable-away-and-neither-is-its-give-up).

## Decision X6: a reliable provisional is one record, and the PRACK this stack owes is the engine's

- `Call.provisionals` holds one `Provisional` per reliable provisional received, with what the
  responder is owed and, once shown, where and how.
- The engine records a reliable provisional before the rules, and drops a repeat (§3) or an
  out-of-order one (§4). A rule only chooses to relay or absorb.
- `settle_owed_pracks` PRACKs what is `Pending` and shown to no one live, inside the primitives
  that end a dialog or an INVITE and once more after the rules. Never twice per `RSeq`.
- `prack_disposition` gives `Relay`, `Answer200` or `Refuse481` to every received PRACK.
- `Dialog.owed_answer` names the one offer a dialog this stack dialled still owes an answer to.
- A provisional is shown reliably only to an INVITE that offered `100rel`.

Per-turn invariants hold the step to its contract. Detail:
[appendix X6](appendix/0032-one-home-for-retransmission.md#decision-x6-a-reliable-provisional-is-one-record-and-the-prack-this-stack-owes-is-the-engines).

## Consequences

- The `pivot-interpreter` audit model paces with `sip-retransmit`, so the SUT and its oracle
  cannot disagree about a schedule.
- ADR-0007 and ADR-0010 keep their decisions. The dialog-level retransmission obligation sits
  in the framework, and both layers share one schedule.
- ADR-0014's reactive-only takeover holds: a node restoring the call holds the rung the last
  write held.

Tests: [appendix, pinned by](appendix/0032-one-home-for-retransmission.md#pinned-by).
