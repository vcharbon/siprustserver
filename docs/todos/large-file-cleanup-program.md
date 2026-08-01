# Large-file cleanup & module-split program

Goal: no source file over ~500 lines; every crate discoverable from its
`lib.rs` without reading implementation files; comments state present-tense
contracts only.

Prior art — copy the shape of these commits (API unchanged, `pub use`
re-exports at the old path, one commit per file):

- `2226086` + `f87093f` — `b2bua/src/router.rs` → `router/` (11 files)
- `b4ff15a` — `loadgen/src/mux.rs` → `mux/` (7 files)

## Per-file procedure

Do the steps in this order for each entry; one file (or one listed group) per
commit.

1. **Map the public surface.** List every `pub` item and grep who uses it
   (`grep -rE 'ident' crates --include='*.rs'`). Unused `pub` items get
   demoted to `pub(crate)` or deleted, not carried over.
2. **Design the split.** One concern per module file, each ideally <500
   lines. Split by domain noun, never into `util.rs`/`helpers.rs` grab-bags —
   a grab-bag is the disease this program cures. The old path keeps working:
   the new `mod.rs` re-exports the surviving public surface.
3. **Comment pass** (rules below) on every module as it is carved out.
4. **Write the discoverability layer** (rules below): module `//!` headers +
   the crate `lib.rs` index.
5. **Verify.** `cargo check` then the default test lane, capped and ONE
   compile at a time per CLAUDE.md:
   `systemd-run --user --scope -q -p MemoryMax=12G -p CPUQuota=1200% nice -n 10 cargo test --workspace --jobs 6`.
6. **Commit** (`refactor(<crate>): split <file> into modules; comment scrub`),
   tick the checklist here, append any raised suspicions to the log at the
   bottom.

## Comment rules (delete / rewrite / raise)

**Delete on sight**

- History: dates, "previously / used to / no longer / originally /
  replaces", commit hashes, ticket IDs. Git and ADRs own history.
- TS/JS-port references ("mirrors the TS version", "like sipjsserver",
  "ported from"). The Rust code has diverged; the comparison is dead weight.
  ~36 such comments remain workspace-wide
  (`grep -rniE 'typescript|sipjs|ported from' crates --include='*.rs'`).
  If the comparison encodes a live invariant (e.g. "deliberately simpler
  than the TS clock pump"), rewrite it as a present-tense contract with no
  TS mention; the test-clock one is already documented in
  docs/testing/test-clock.md — a doc pointer suffices.
- Narration ("now we parse the header", "call the helper"), restated
  signatures, and PR-reviewer talk ("this is safe because we just checked").

**Keep / rewrite**

- Present-tense contracts and invariants. If a rationale needs >~5 lines, it
  goes to an ADR with a one-line pointer (CLAUDE.md rule).
- RFC section citations on behavior (`§17.2.1`) — these are contracts, keep.

**Raise, don't silently delete**

A *suspicious comment* justifies behavior instead of describing it —
especially non-RFC-compliant output on the SUT side, or weakening of an HA
invariant (time-based settle, wire-touching reclaim, non-pristine reboot —
see docs/testing/ha-acceptance.md). Do NOT fix the behavior in the same
commit as the split. Append it to the **Raised suspicions** log below with
file:line, the quoted comment, and a one-line assessment; leave the code
as-is (or add `FIXME(scope):` only if the defect is unambiguous).

**Danger zones — read the doc before touching**

- `sip-message` is the ONLY crate allowed to extract SIP headers/messages.
  While splitting other crates, any inline header-poking you find is a
  finding — log it, don't replicate it.
- `b2bua/src/initial_invite.rs`, `invariants::enforce`, proxy response path
  (`sip-proxy/src/core/*`): read ADR-0022 first.
- Anything under `b2bua/src/repl/`, `failover-harness/`: read
  docs/testing/ha-acceptance.md first.
- Any timed test you rearrange: docs/testing/test-clock.md.

## Discoverability rules (the "where do I find a method on X" layer)

The goal: a consumer answers "does a function for this already exist?" by
reading at most two screens — the crate `lib.rs` index, then one module
header — never an implementation file.

1. **Crate `lib.rs` is a table of contents.** Grouped `pub use` blocks, each
   group preceded by a one-line comment naming the concern. No logic in
   `lib.rs` beyond wiring.
2. **Every module opens with a `//!` header, 2–4 lines**: what this module
   owns, and one "does NOT live here → see X" pointer when there is a
   plausible wrong guess (e.g. `//! Raw SIP scanning does NOT live here —
   see sip_message::sniff`).
3. **One concern per file** (CLAUDE.md rule). If a function operates on type
   `T`, it lives in `T`'s module or an explicitly named extension module —
   never in a sibling's file because it was convenient.
4. **Splitting a `*_helpers` / grab-bag file**: bucket by the type the
   functions act on, make each bucket a module, and leave the old module
   name as a thin re-export façade so call sites don't churn (they can be
   migrated opportunistically later).

## Session sizing

One file (or one listed group) per session for Lane 1–2 entries — the split +
consumer sweep + verification of a 1500+ line hub consumes a full context
window, and comment judgment degrades when it's shared. Lane 3–4 entries
(~500–700 L, self-contained) may be batched two or three per session.

## Order & tracking

Ranked by (fan-in × size × comment-smell). Fan-in = files outside the crate
that import from the crate; smells = historical/suspicious comment count at
scan time (2026-07-15). Sizes are line counts at scan time.

### Lane 1 — shared-vocabulary crates (highest duplication-prevention payoff)

- [x] **1. `crates/sip-message/src/message_helpers.rs`** — DONE 2026-07-15:
  1695 L → `message_helpers/` (10 files, largest 471 L, all public paths
  unchanged via mod.rs re-exports). Concerns: headers / name_addr / uri /
  via / param_codec / emergency / preparse / reject_503 / bytes. All 36
  TS-port references in the crate scrubbed (0 remain); lib.rs is now the
  extraction-authority index; sniff.rs cross-points to preparse. Four
  suspicions raised (see log).
- [x] **2. `crates/scenario-harness/src/agent.rs`** — DONE 2026-07-16:
  4083 L → `agent/` (17 files, largest 512 L, all public paths unchanged via
  mod.rs re-exports; lib.rs re-export list untouched). Concerns: harness /
  run_guards / step / ua / tolerant_recv / txn_view / invite / client_invite /
  out_of_dialog / dialog / server_txn / client_txn / proxy / rr_fold /
  extract / tests. All TS-port + ticket-ID comments scrubbed (incl. the
  crate lib.rs header); crate-internal seams (`Ids`, `TxnView`,
  `AckObligations`, `decide_rr_fold`, `top_via_branch`,
  `InviteResponseFate`) re-exported `pub(crate)` from mod.rs for loadbind /
  callee_group / actor. Header-extraction residue quarantined in
  `agent/extract.rs` (see suspicions).
- [x] **3. `crates/sip-message/src/generators.rs`** — DONE 2026-07-24:
  912 L → `generators/` (9 files + mod, largest 139 L, all public paths
  unchanged via mod.rs re-exports). Concerns: spec / methods / emit (crate-
  internal) / out_of_dialog / in_dialog / ack / cancel / response / relay.
  Vocabulary aligned with #1: the read/rewrite items that lived in
  generators moved to message_helpers — `first_route_is_loose` +
  `strip_route_uri_to_request_uri` → new `message_helpers/route.rs`,
  `stamp_received_rport_on_via` → `message_helpers/via.rs` — with
  `generators::` re-exports keeping every old consumer path. TS-port +
  slice-2 port-plan comments scrubbed; `build_via_value`/`build_contact_value`
  became `ViaSpec::header_value`/`ContactSpec::header_value` (pub(crate)).
  One suspicion raised (see log).
- [x] **4. `crates/call/src/model.rs` + `crates/call/src/helpers.rs`** —
  DONE 2026-07-24: 833 L → `model/` (8 files + mod, largest 209 L) and
  756 L → `helpers/` (7 files + mod, largest 196 L); all public paths
  unchanged via mod.rs re-exports. model concerns: record / leg / dialog /
  invite_txn / timer / cdr / services / sm; helpers bucketed by the type
  acted on (rule 4): lens / leg / dialog / peering / services / record /
  timer. Dead pub helpers deleted: `transfer_phase`,
  `a_leg_invite_cseq_num` (see log). lib.rs rewritten as grouped TOC;
  TS-port comments + upstreamneed/GAP ticket IDs scrubbed across src
  (incl. features.rs / codec.rs / callref.rs / Cargo.toml description);
  the crate's *test* files still carry TS wire-parity comments — Lane 4.
  Gotcha: a submodule named `call` inside the `call` crate makes
  `use call::model::*;` shadow the extern crate at every glob-import site
  (E0659), so the master-record modules are named `record.rs`.
- [x] **5. `crates/sip-txn/src/layer.rs`** — DONE 2026-07-24: 1644 L →
  `layer/` (6 files + mod, largest 417 L; all public paths unchanged via
  mod.rs re-exports, and every consumer already imported via the crate root).
  Concerns: handle (public API + Command funnel) / owner (select! loop +
  lockstep map/wheel bookkeeping + sweep) / client (§17.1 UAC FSM incl.
  non-2xx auto-ACK + Timer D hold + per-call eviction) / server (§17.2 UAS
  FSM incl. Timer-G non-2xx final retransmit — RFC citations kept) / events
  (lossy `emit` vs lossless `emit_critical` + ADR-0014 CallQuiesced
  end-of-turn ordering) / txn (per-transaction state + sweep-age policy).
  Dead items deleted (rule 1): `TxnState::Terminated` (never set) and
  `Transaction.method` (never read) — both justified only as source-FSM
  fidelity. TS-port comments scrubbed crate-wide (lib.rs → grouped TOC,
  event.rs, timers.rs, rng.rs, metrics.rs, Cargo.toml description + dev-dep
  note); the crate's test files untouched — Lane 4. No new suspicions.

### Lane 2 — hot paths & rule engines

- [x] **6. `crates/sip-proxy/src/core/request.rs`** — DONE 2026-07-24:
  1630 L → `core/request/` (9 files, largest 459 L; entry points stay
  crate-internal — `route_request` is `pub(in crate::core)`, nothing was
  ever exported past the crate, so zero consumer churn). Concerns: mod
  (RouteOutcome + `top_via_branch` correlator + `handle_request` metering
  shell) / route (the §16 ladder) / record_route (double-RR insertion,
  carved out of the ladder as `insert_double_record_route`) / reply
  (self-generated UAS finals + `ackhop|` absorb memo + select-failure 503);
  the five embedded test modules became sibling files (worker_outbound /
  retransmission / cookie_identity / rfc_small_fix / ack_hop). ADR-0022 X4
  contract comments kept verbatim. Comment pass on `core/mod.rs`,
  `core/response.rs`, the crate `lib.rs` header (TS-port/slice framing →
  present-tense scope), `cancel_lru.rs` + `tests/self_gate_admission.rs`
  stale-path/ticket refs. All ProxyCore.ts / migration/14 / upstreamneed
  ticket comments in src/core + lib.rs scrubbed. No new suspicions
  (headers.rs is documented as proxy *policy* composers over sip-message
  primitives — no extraction violation).
- [x] **7. `crates/b2bua/src/rules/actions.rs`** — DONE 2026-07-25: 2295 L →
  `actions/` (9 files, largest 402 L; the only public item, `ActionExecutor`,
  re-exported unchanged from mod.rs — zero consumer churn). Concerns: mod
  (struct + `execute` + the ONE timer-schedule recipe) / dispatch (the
  `RuleAction` → handler table; state-mutation arms inline, everything that
  builds SIP delegates — the exhaustive 46-variant match is 328 code lines
  and stays on the `too_many_lines` warn ratchet, same as
  `defaults::core_rules`) / relay_request (incl. `ack_leg`, deduped from the
  AckLeg arm + relay ACK branch) / relay_response (incl. bare-180 downgrade)
  / dialog_track / originate (CreateLeg admission gate + B2BUA-originated
  requests) / respond (a-facing finals/provisionals/2xx-retransmits) /
  teardown / select (leg/dialog selection views). TS-port (`ActionExecutor.ts`
  ports, "mirrors the TS leg iteration") + ticket-ID (GAP-P7-1, GAP-P8b-2,
  upstreamneed-021/027/028) comments scrubbed; doubled `#[allow]` removed. One
  suspicion raised (see log #10: wire-reader extraction residue).
  `rules/defaults.rs` (1407 L), `relay.rs` (1039 L), `refer_transfer.rs`
  (1041 L) still follow in this lane.
- [x] **8. `crates/scenario-harness/src/actor/actor.rs` + `actor/mod.rs`**
  — DONE 2026-07-26 (grown to 2898 + 4043 L since scan). actor.rs dissolved
  into nine sibling concern modules, largest 475 L; every `actor::` public
  path unchanged (mod.rs re-exports from `endpoint` + `runner`). Concerns:
  endpoint (declarative vocabulary — Disposition / MediaState / CtxFeed /
  ActorSpec / SUBFLOW names + the declarative-feeding contract) / runner
  (ActorState + holder structs + the `select!` loop; fields `pub(super)`) /
  react (reactive request answer table + CANCEL automatic) / answer (UAS
  answer/reject primitives + disposition entry) / response (reactive
  response fold: establish, fork late-2xx, glare completion, obligation
  closing) / drive (goal-step dispatcher) / originate (every sent request +
  glare-retry wait arms) / script (Scripted parking + reception/respond
  goals) / accept_delta (ADR-0024 §6 consult). mod.rs's 3.8 kL embedded
  test module became `actor/tests/` — 12 concern files + a shared `testkit`
  (substrate / forking / cancel_race / reneg / auth / scripted_replay /
  scripted_cancel / delta_request / delta_response / script_flow /
  template_request / template_respond). Comment scrub: impl-plan review IDs
  (B1–B7), build-phase codes (P0/P1/P3), ticket refs (036/044/047,
  upstreamneed), design-artifact/memory pointers → present-tense contracts;
  the C*/S*/E* scenario-shape codes KEPT (living vocabulary shared with
  `crates/callshapes`). `scenarios.rs` (1049 L) is still >500 — outside
  this entry's scope, queue it with the Lane 3 remainder.
- [x] **9. `crates/sip-proxy/src/load_observer.rs`** — DONE 2026-07-26:
  1157 L → `load_observer/` (5 files + mod, largest 398 L; all surviving
  public paths unchanged via mod.rs re-exports). Concerns: payload (the
  `X-Overload` value codec — header *extraction* stays in sip-message) /
  band (EluBand + the hysteresis walk, moved beside the thresholds that
  drive it) / config (tunables + `validate_bands` + the calibration-
  starting-point defaults) / observer (WorkerState + AIMD ladder + token
  bucket + sweep + snapshot) / tests (the observer-driven suite; pure
  parse/validator tests inline in their modules — all 34 kept). Dead pub
  deleted (rule 1): `note_rejection_payload` — unwired on BOTH wire ends
  (see log #11). `AimdAction`/`AimdSnapshot`/`snapshot` stay pub: the
  diagnostics surface is test-only today but demotion just trades API
  for dead-code allows. TS-port comments scrubbed (module header,
  every "port of TS x" method doc, migration/32 refs in Default +
  test assertions, `it("…")` test-name citations); the stale
  "load_observer.rs TODO(metrics)" pointer + `bin/proxy.ts` /
  HealthProbe.ts / LoadBalancer.ts refs in sip-proxy-runner main.rs,
  health/probe.rs and strategies/load_balancer.rs rewritten present-tense.
  The explicit-`now_ms` clock contract kept as the module-doc centerpiece.
- [x] **10. `crates/b2bua/src/overload.rs`** — DONE 2026-07-26: 1129 L →
  `overload/` (7 files + mod, largest 300 L; all public paths unchanged via
  mod.rs re-exports). Concerns: sampler (the LoadSampler read seam — live
  tokio busy-ratio + the injectable simulated pair) / ewma / bucket (the
  CPS TokenBucket + its tokio::time clock contract) / admission (verdict
  types + tunables + seed defaults) / signal (OverloadSignal: EWMAs,
  counters, header builder, should_admit) / prometheus (text exposition) /
  tests (the signal-driven suite; primitive tests inline beside their
  module — the externally referenced names `the_bucket_refills_over_time`
  and `panic_elu_*` stay under `overload::tests`). Rule 1: `LiveLoadSampler`
  demoted to pub(super) (zero code consumers outside the module; only
  `OverloadSignal::live` constructs it) and its dead `_sample_window`
  parameter deleted. TS-port (OverloadController.ts / LoadSampler.ts /
  toFixed(3) / it.live) + migration/08-09-32 + slice refs scrubbed, incl.
  the six overload-seam comment sites in b2bua_core.rs; the stale module-doc
  claim that the token bucket / shouldAdmit gate "is intentionally absent
  here" corrected (both live here now). The tokio::time-vs-real-wall clock
  contract kept as the mod.rs centerpiece. No new suspicions.

### Lane 3 — big but self-contained (internal fan-in only)

- [ ] **11. `crates/sip-net/src/rfc_audit/rfc3261_cross.rs`** — 3028 L.
  Plus siblings `starter_peer.rs` (1866), `rfc3262_cross.rs` (1820),
  `cross_generic.rs` (1404), `rfc3264_cross.rs` (1043), `dialog_model.rs`
  (995). Internal to the audit, but this suite gates EVERY test — split
  rule-family-per-file so a failing rule name maps to one file. Waiver
  text (`allow_violation` justifications) is contract, not history — keep.
- [ ] **12. `crates/failover-harness/src/harness.rs`** — 1738 L, 13 smells.
  ha-acceptance.md danger zone.
- [ ] **13. `crates/b2bua/src/metrics.rs`** — 1198 L.
- [ ] **14. `crates/b2bua-sdk/src/model.rs`** — 1120 L.
- [ ] **15. `crates/b2bua/src/decision/test_adapter.rs`** — 1093 L.
- [ ] **16. `crates/loadgen/src/report.rs`** — 974 L; then `driver.rs`
  (808), `app.rs` (791).
- [ ] **17. `crates/e2e-web/src/lib.rs`** — 1255 L and
  `crates/sip-proxy-runner/src/main.rs` — 1253 L. Runner policy belongs in
  b2bua-runner-kit, not inline in a main.rs — moving logic out may beat
  splitting in place.
- [ ] **18. `crates/b2bua/src/repl/puller.rs`** (930),
  `repl/supervisor.rs` (709), `repl/store.rs` (535) — ha-acceptance danger
  zone; comment scrub mandatory, splits only where a seam is obvious.
- [ ] **19. Remainder under 1000 L** — `b2bua-runner-kit/src/lib.rs` (877),
  `sip-proxy/src/resolver.rs` (842), `b2bua/src/store/mod.rs` (826),
  `b2bua-harness/src/lib.rs` (813), `sip-proxy/src/self_gate.rs` (801),
  `callshapes/src/plan.rs` (912), `sip-net/src/contracts.rs` (767),
  `e2e-model/src/registry.rs` (723), `seq-report/src/lib.rs` (660),
  `sip-pcap/src/lib.rs` (664), `topology/src/lib.rs` (605),
  `sip-message/src/parser/custom/structured_headers.rs` (956),
  `extract_fields.rs` (725), `sdp.rs` (581), `media/src/transport.rs`
  (560), `e2e-core/src/infra.rs` (606) + `registrar.rs` (587),
  `sip-proxy/src/observability/metrics.rs` (677),
  `seq-report/src/html.rs` (631), `sip-pcap/src/bin/sipflow.rs` (646),
  `b2bua/src/b2bua_core.rs` (646), `b2bua/src/timers.rs` (628 — module doc
  is load-bearing per test-clock.md, keep it),
  `b2bua/src/initial_invite.rs` (525 — ADR-0022).

### Lane 4 — test files (comment scrub yes, splitting optional)

Tests don't have a public API, so the discoverability payoff is small; scrub
comments and split only when navigation genuinely hurts.

- [ ] `crates/loadgen/tests/smoke.rs` (2533), `crates/b2bua/tests/rules.rs`
  (2044), `crates/failover-harness/tests/failover.rs` (1533),
  `crates/b2bua/src/repl/real_transport_tests.rs` (1227) and the repl
  `s*_tests.rs` files, `crates/sip-txn/tests/fsm.rs` (818), the rest of
  the >500 L test files.

### Done (before this program file existed)

- [x] `b2bua/src/router.rs` → `router/` — 2226086 + f87093f
- [x] `loadgen/src/mux.rs` → `mux/` — b4ff15a (incl. `sip_message::sniff`
  extraction)

## Raised suspicions log

Append entries as found; never delete an entry, mark it `resolved:` instead.

### 2026-07-15 — message_helpers split

1. **`message_helpers/emergency.rs` — `buffer_has_emergency_marker`
   matches the `Resource-Priority` header NAME case-sensitively.** RFC 3261
   §7.3.1 makes header names case-insensitive, so a genuine emergency INVITE
   written `RESOURCE-PRIORITY: esnet.0` is shed by the Tier-1 brake under
   overload — the exact outcome the "NEVER 503 an emergency" contract
   forbids. The old comment justified it as "the upstream contract requires
   canonical casing, per docs/overload-protection.md" — that doc exists only
   in the retired TS repo, not here.
   `resolved:` 2026-07-15 — user confirmed the casing was a TS-parser
   workaround. Byte scan rewritten as a header-section line walk with
   case-insensitive name match (and now body-spoof-proof); the b2bua
   `initial_invite` pin updated.
2. **`emergency.rs` — RPH value tokens matched case-sensitively**
   (`esnet.0`, not `ESNET.0`) in both `is_emergency_request` and the byte
   scan. RFC 4412 namespace names are case-insensitive. Same TS-doc
   justification chain as #1.
   `resolved:` 2026-07-15 — same commit. `is_emergency_request` now reads
   every RP header as a comma-split r-value list, trimmed, compared
   case-insensitively (whole r-value, so `esnet.01` no longer matches);
   byte scan is case-insensitive substring within the field. The proxy's
   duplicate classifier (`strategies/load_balancer.rs::is_emergency_invite`)
   now delegates to the sip-message implementation.
3. **`message_helpers/preparse.rs` — `buffer_has_to_tag` matches `To`/`t`
   case-sensitively** at line start; a `to:` header classifies an in-dialog
   request as initial. Latent: zero consumers today (the dispatcher
   fast-path it was built for was never wired).
   `resolved:` 2026-07-15 — deleted per user (dead code; the lenient
   `sniff::to_tag` covers real To-tag reads if a future fast-path needs one).
4. **`message_helpers/reject_503.rs` — first-line guard only checks for the
   `SIP/2.0` substring**, so a *response* datagram fed to
   `build_stateless_reject_503_buffer` would be templated into a 503 reply.
   Benign today (the brake only feeds it requests); noted in
   `first_line_without_sip_version_returns_none`.

### 2026-07-16 — scenario-harness agent split

5. **`agent/extract.rs` — the harness carries its own SIP header/URI readers**
   (`top_via_branch`, `top_via_addr`, `unwrap_angle`, `first_contact_uri`,
   `rack_for`, `uri_to_addr`, `hostport_to_addr`, plus an inline Via sent-by
   split in `agent/proxy.rs::strip_top_via_if_self`), violating the
   sip-message-only extraction rule. `sip_message` already exposes structured
   equivalents (`parse_via` carries host/port/branch;
   `name_addr`/`extract_contact_uri`; `uri::extract_host_port`). Consolidated
   into ONE marked module during the split; migration onto the sip-message
   readers is the follow-up commit.
   `resolved:` 2026-07-16 — follow-up commit: `agent/extract.rs` →
   `agent/addressing.rs`, all parsing delegated (`parse_via_params`, new
   `via_sent_by` added to `message_helpers::via`, `extract_host_port`,
   `extract_contact_uri`); `unwrap_angle` deleted; `rack_for` +
   `first_contact_uri` moved to their sole consumer (`client_invite.rs`).
   Gotcha kept as a scheme-prefix guard in `uri_to_addr`:
   `parse_sip_uri_string` reads everything before the first `:` as a scheme,
   so a bare `host:port` must not be fed to it.
6. **`agent/ua.rs::quiesce` answers EVERY queued request with a bodyless
   `200 OK`** — including an offer-carrying re-INVITE/UPDATE (RFC 3264 §5
   forbids the answerless 200) and even an ACK (which takes no response at
   all). Confined to the load driver's failed-call teardown window (those
   calls are never RFC-audited), and
   `try_receive_tolerating_blocking` exists as the assertable, compliant
   replacement — but any new use of `quiesce` on an audited path would emit
   non-compliant peer SIP. Candidate fix: skip ACKs and attach `ANSWER_SDP`
   to offer-carrying INVITEs/UPDATEs inside `quiesce` itself.
   `resolved:` 2026-07-16 — the blind primitive was hiding TWO distinct
   situations. Split by lane: `Agent::release_failed_call` (failed-call
   teardown, load driver) answers per a stateless-UA decision table
   (`release_verdict`: ACK→absorb, BYE/CANCEL→200, INVITE/UPDATE→481,
   other-in-dialog→481, out-of-dialog probe→200 — so a queued INVITE
   retransmit can no longer be 200'd into a zombie leg); `Agent::drain_expecting`
   (successful multi-leg transfer straggler drain, functional tests) answers
   ONLY the whitelisted method (BYE) via a real `ServerTxn` 200 and PANICS on
   anything else, because those legs are still live dialog members whose
   behavior MUST stay asserted. `quiesce` deleted; both call sites migrated;
   pin tests in `tolerant_recv::release_verdict_tests`.
7. **`InDialogRequest::with_to_tag` doc said the shared CSeq counter still
   advances and per-fork CSeq independence is not asserted** — stale: the
   send path forks an independent per-fork CSeq sequence whenever the fork
   map is wired (`ClientInvite::send_request`). Comment rewritten to the
   actual contract during the split; no behavior change.

### 2026-07-24 — generators split

8. **`generators/in_dialog.rs` + `generators/ack.rs` — an in-dialog request /
   2xx-ACK built from a dialog with an EMPTY `remote_tag` omits the To tag
   entirely** (RFC 3261 §12.2.1.1 requires it). The old ~10-line comments
   justified this as the mid-confirm failover-takeover hydrate path (a replica
   copy whose relayed 2xx had not yet established the remote tag): emitting
   `;tag=` with an empty value is malformed and panics `hydrate_request`.
   Assessed deliberate degenerate-path safety — well-formed-but-tagless beats
   a panicking worker, and "the degenerate dialog is handled elsewhere" — so
   behavior kept; comments condensed to the contract. Worth a check some day
   that the recorded-trace audit would flag a tagless in-dialog request if
   this path ever fired outside the takeover corner.

### 2026-07-24 — call model/helpers split

9. **`helpers.rs::a_leg_invite_cseq_num` parsed the CSeq header value inside
   the `call` crate** (name match + `split_whitespace().parse()` over the
   retained a-leg INVITE's `SipHeader` list) — SIP header extraction outside
   `sip-message`, in a crate whose stated design is "SIP payloads stay raw
   bytes, no sip-message dep". Zero non-test consumers.
   `resolved:` 2026-07-24 — deleted in the split (dead code, per procedure
   rule 1). If a future consumer needs the snapshot's CSeq, derive it via
   sip-message at the call site instead of re-adding value parsing here.

### 2026-07-25 — b2bua rules/actions split

10. **The action executor carries four hand-rolled SIP wire readers** —
    extraction outside sip-message (same class as #5). Kept verbatim in the
    split, each private beside its sole consumer: `via_sent_by` + the
    `top_via_dest` twin (whitespace-split Via sent-by;
    `actions/relay_response.rs`, `actions/respond.rs`), `unwrap_angle`
    (angle-bracket Contact unwrap; `actions/dialog_track.rs`, 3 call sites),
    `rewrite_rack` (RAck middle-token rewrite; `actions/relay_request.rs`).
    sip-message already exposes structured equivalents:
    `message_helpers::via::via_sent_by`, `name_addr::extract_contact_uri`,
    `parse_rack`. Migration is NOT byte-neutral — e.g. `unwrap_angle` keeps
    `;params` on a non-angle Contact where `parse_contact` splits them off
    the URI — so it needs its own commit with the delta reasoned per site.
    `resolved:` 2026-08-01 — dissolved by the ADR-0025 header-model port
    (merge bce0d4b): `relay_request.rs` reads the `RAck` header type,
    `respond.rs` reads `req.top_via()`, and `dialog_track.rs` reads the
    structured Contact, so `top_via_dest`, `unwrap_angle` and `rewrite_rack`
    have no remaining call sites. The one survivor is compliant: `via_sent_by`
    in `actions/relay_response.rs` is a thin wrapper over sip-message
    `Via::parse` — the pending-request snapshot stores its Vias as text (the
    `call` crate has no sip-message dependency), so reading one back is a
    parse, not a hand-rolled reader. A sweep of the whole of `rules/` for
    header-value splitting, angle unwrapping and RAck/CSeq token splitting
    found no other **header-level** reader; the same sweep did surface two
    body-level ones, logged as #12 and #13.

### 2026-07-26 — sip-proxy load_observer split

11. **The X-Overload-on-503 fast path is unwired on BOTH ends of the wire.**
    `WorkerLoadObserver::note_rejection_payload` ("a payload that rode a 503
    reply to a forwarded INVITE") had zero callers, and the worker side
    deliberately stamps `X-Overload` on the OPTIONS **200 path only** — a 503
    already removes the node from selection, so the band signal is not stamped
    there (pinned by `options_200_stamps_x_overload_503_does_not`, tracked in
    b2bua `router/responses.rs` as a divergence to revisit with the AIMD
    rate-cap consumer). Deleted the dead entry point per procedure rule 1; if
    the fast path is ever wired, re-adding a distinct entry beside
    `apply_payload` is one line, and the real work is (a) stamping the header
    on worker 503s and (b) teaching the LB response path to sniff relayed 503s
    without violating its transaction-less design (ADR-0022 X4).

### 2026-08-01 — b2bua rules wire-reader sweep

12. **`rules/sdp_answer.rs` was a 322-line second copy of the RFC 3264 answer
    builder that already lives in `sip_message::sdp`** — body extraction
    outside sip-message, and a fork that had drifted from the original.
    `resolved:` 2026-08-01 — deleted; the sole consumer
    (`relay_first_18x.rs`, the fake-PRACK UPDATE handler) now calls
    `sip_message::build_answer_from_offer` with `BuildAnswerOptions`, and the
    b2bua unit tests are subsumed by `sip-message/tests/sdp_answer.rs` (12
    cases against 3). The switch is byte-neutral on well-formed SDP; on
    malformed input the four deltas do NOT all point the same way — three
    widen or are inert, one narrows:

    1. *widens* — JS `parseInt` semantics. The m-line **port** reads `5004x`
       as 5004 where the fork's `parse::<i64>()` failed and fell back to 0;
       a payload type reads `96x` as 96 where the fork's
       `filter_map(parse::<i64>().ok())` dropped the token from the list
       entirely (it never became a 0 — that fallback was port-only).
    2. *tie-break only* — `a=rtpmap`/`a=fmtp` are keyed by payload type in a
       map, so a repeated PT is last-wins; the fork's `Vec` + `find` was
       first-wins. Neither is more lenient.
    3. **narrows** — lines split on `\n` with an optional trailing `\r`,
       where the fork split on either character. A bare-CR-separated body no
       longer parses into lines: if the body has no `\n`-terminated `m=`
       line at all, `media_sections` comes back empty and the result is
       `NoAliceSdp`; if the `m=` line opens the body, `split_whitespace`
       absorbs the following `a=rtpmap:` text into the m-line token list,
       the dynamic PT loses its codec name, `codec_key` returns `None` and
       the result is `NoCommonCodec`. Either way the fake-PRACK UPDATE
       handler now replies 488 where the fork replied 200 with an answer.
       That body is malformed under RFC 4566 §5 and the stricter reading is
       the correct one — it is the only delta that can turn a 200 into a
       488, so it is called out here rather than being lumped in.
    4. *inert* — session-level `a=x-offer-id:` attributes are echoed into
       the answer; no peer in this repo emits one.

    The commit message (24e574e) states delta 1's payload-type case and
    delta 3's direction wrongly; this entry is the correct record.

13. **`rules/sdp_diff.rs` hand-parsed SDP bodies into `m=` blocks** — same
    violation class as #12 (body extraction outside sip-message), and it
    carried a second, divergent SDP line-splitter: it split on either `\r` or
    `\n`, the very semantics #12 retired for the answer builder.
    `resolved:` 2026-08-01 — moved to `crates/sip-message/src/sdp_diff.rs`
    (`sip_message::sdp_media_equivalent`, re-exported at the crate root) and
    rebuilt on `sdp::split_lines`, so one splitter now serves every SDP
    reader. This is a move, not a swap: sip-message had no comparator, and
    the sole consumer (`rules/promote_pem.rs`, the 183→200 resync decision)
    is unchanged apart from the import. Two byte-level deltas, both on
    malformed input: a bare-CR-separated body now yields zero `m=` blocks, so
    it compares equal to any other block-less body (suppressing the resync
    re-INVITE) and unequal to any well-formed one (forcing it); and each
    line keeps its interior text but is compared `trim_end`-ed exactly as
    before, so trailing-whitespace tolerance is preserved.
