# Scenario harness: recording-first, thin DSL (`scenario-harness`)

**Status:** accepted (2026-05-31)

**Source release:** sipjsserver @ submodule `c74e62da7c9bd8e04e183a8b074cad8029daa946`
(`portsource/sipjsserver`). Ported surface: `src/test-harness/framework/{dsl,
interpreter,recorder,svg-sequence-diagram,text-report,html-report}.ts`,
`tests/harness/runner.ts`.

## Context

The source test harness is a fluent DSL
(`alice.invite(...).expect(200).dialog.ack().bye()`) over a two-phase engine:
a `recorder` turns DSL calls into a `Step[]` AST, and a ~2300-line
`interpreter` replays it. The interpreter maintains its own dialog state
(CSeq, route sets, tags, RFC 3264 offer/answer, forking) in `message-builder`
**and builds its own `trace: TraceEntry[]`**, which the SVG / `global.txt` /
per-endpoint / HTML reports then render.

Two facts make a faithful port the wrong move right now:

1. **The machinery it needs isn't ported.** The dialog/CSeq/route-set/
   offer-answer engine *is* the transaction + call-context layers (migration
   slices 3–6, all pending), and there is **no SUT (B2BUA)** to drive against.
   A faithful interpreter would force porting layers that don't exist, against
   nothing to test.
2. **The harness predates the recording layer.** That is why the interpreter
   hand-rolls a parallel `trace`. We now have a recording layer:
   `sip-net`'s `RecordingSignalingNetwork` tees every `send_to`/`recv` onto the
   `layer-harness` `Recorder`. So the trace already exists — as recorded data.

This confirms MIGRATION_PLAN_B2B §4(ii) decision **B** (a slim Rust DSL,
scenarios-as-data) over a close replica.

## Decisions

- **Recording-first: the record *is* the trace.** Pseudo-agents (`alice`,
  `bob`) bind on the recording-wrapped simulated `SignalingNetwork` and
  send/recv real wire bytes. The reports are **projected** from the recorded
  `SignalingNetworkEvent` channel + the recorder's lane registry — never from
  interpreter-maintained state. The driver holds **no trace and no dialog
  state**.
- **The wire projection lives in `sip-net`** (`report::to_sip_entries`), the
  home `layer-harness/src/scenario.rs` already names for it. It pairs each
  `SendCalled` with its delivering `RecvItem` into a byte-level
  `RecordedSipEntry { from, to, raw, sent_ms, received_ms, delivered, seq }`.
  It owns no SIP parser (keeping it in the network layer); a reporter parses
  `raw` itself for labels.
- **A thin DSL in a new `scenario-harness` crate** (test-only, like
  `layer-harness`): named agents + a flat `Vec<Step>` of `Send` / `Expect` /
  `Advance`, and a `run()` driver. The renderers (`svg`, `text`, `html`,
  `wire`) port the source's report format, consuming `RecordedSipEntry` +
  `RecordedScenario`.
- **Report timestamps ride the clock seam.** `layer-harness`'s `Recorder` now
  stamps channel `at_ms` via an injected `sip-clock` `Clock`
  (`Recorder::with_clock`), so under a paused tokio runtime the report's
  relative-time labels advance with `tokio::time::advance` — deterministic,
  per [ADR-0004]'s note and the TODO in `layer-harness/src/time.rs`. The
  harness anchors `Clock::test_at(0)`. `seq` remains the ordering authority;
  the fabric's `arrival_ms` and decorators' anomaly `at_ms` (neither rendered
  as a timeline) keep the wall-clock `time::now_ms()`.

## The fluent dialog builder (added 2026-05-31)

The thin data-DSL alone meant scenarios hand-authored every byte. That does not
scale, so the **fluent, dialog-aware layer** (`agent.rs`) was added as the
*primary* surface — the port of `recorder.ts`'s `AgentProxy`/`DialogRef` + the
dialog state in `message-builder.ts`. It is built on `sip-message::generators`
(the correct-by-default B2B builders ported in slice 1) plus the `StackDialog`
state shape:

- `Harness` owns the recording-wrapped simulated network; `agent(name, addr)`
  binds a stateful UA.
- `alice.invite(&bob).with_sdp(..).send()` → `ClientInvite`; `.expect(180/200)`
  learns the remote tag + remote target from the response; `.ack()` →
  `Dialog`; `dialog.bye()`/`request(..)` auto-increment CSeq.
- `bob.receive("INVITE")` → `ServerTxn`; `.respond(200,"OK").with_sdp(..)`
  echoes Via/From/To/Call-ID/CSeq and mints a stable To-tag.
- The harness fills in, per RFC 3261: Via (fresh branch per transaction, magic
  cookie), From/To tags, Call-ID continuity, CSeq (1 INVITE → 1 ACK → n BYE;
  responses echo), Contact, Max-Forwards, Content-*; in-dialog requests route to
  the learned remote target.

It stays **recording-first**: messages still flow through the recording network,
so the reports project from the record unchanged — auto-generation only changes
*who writes the bytes*. The low-level `Scenario`/`Send`/`Expect` data-DSL is kept
as the escape hatch for raw/torture cases that must put exact (possibly
malformed) bytes on the wire.

## What is dropped (until the producing layers land)

`or`-branching, `parallel`, media (RTP, ADR-0017), and k8s/chaos steps. The
SUT/tier machinery (`runOn`, tiers) is dropped permanently — there is no SUT
topology to select. See MIGRATION_STATUS.md (Slice "Scenario harness") for the
per-feature justification.

## Consequences

- The harness is usable now to integration-test each future layer as it lands:
  point agents at the layer-under-test instead of at each other.
- When the transaction/dialog layers arrive, the DSL grows convenience
  constructors (`invite`, `ack`, …) that *emit `Send`/`Expect` steps with
  generated messages* — the driver and recording-first projection stay
  unchanged.

[ADR-0004]: ./0004-layer-harness-test-foundation.md
