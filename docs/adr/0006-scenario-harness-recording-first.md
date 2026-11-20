# Scenario harness: recording-first, thin DSL (`scenario-harness`)

**Status:** accepted (2026-05-31)

## Context

A cross-layer scenario test drives named agents (`alice`, `bob`) through a
callflow and renders what crossed the wire as an SVG sequence diagram, a
`global.txt` ladder, per-endpoint views and an HTML report. A harness that
replays a scenario AST through its own interpreter has to keep its own dialog
state (CSeq, route sets, tags, RFC 3264 offer/answer, forking) and build its
own parallel trace for the reports, a second record that can disagree with
the wire.

The recording layer already holds that trace: `sip-net`'s
`RecordingSignalingNetwork` tees every `send_to`/`recv` onto the
`layer-harness` `Recorder`. The dialog state belongs to the transaction and
call-context layers and to the SIP generators, not to a test interpreter.

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
- **A thin DSL in a `scenario-harness` crate** (test-only, like
  `layer-harness`): named agents + a flat `Vec<Step>` of `Send` / `Expect` /
  `Advance`, and a `run()` driver. The renderers (`svg`, `text`, `html`,
  `wire`) consume `RecordedSipEntry` + `RecordedScenario`.
- **Report timestamps ride the clock seam.** `layer-harness`'s `Recorder`
  stamps channel `at_ms` via an injected `sip-clock` `Clock`
  (`Recorder::with_clock`), so under a paused tokio runtime the report's
  relative-time labels advance with `tokio::time::advance` — deterministic,
  per [ADR-0004]'s note and `layer-harness/src/time.rs`. The harness anchors
  `Clock::test_at(0)`. `seq` remains the ordering authority; the decorators'
  anomaly `at_ms` and the fabric's undeliverable-packet stamp (neither rendered
  as a timeline) keep the wall-clock `time::now_ms()`. A packet's
  `arrival_ms` rides the binding endpoint's `Clock`.

## The fluent dialog builder (added 2026-05-31)

The thin data-DSL alone meant scenarios hand-authored every byte. That does not
scale, so the **fluent, dialog-aware layer** (`agent.rs`) is the *primary*
surface. It is built on `sip-message::generators` (the correct-by-default B2B
builders) plus the `StackDialog` state shape:

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

## Consequences

- Agents point at the layer under test (a B2BUA or a proxy bound as the
  system under test) instead of at each other; the driver and the
  recording-first projection stay the same whatever the target.
- A new convenience step emits `Send`/`Expect` steps with generated messages;
  the projection never changes for it.

[ADR-0004]: ./0004-layer-harness-test-foundation.md
