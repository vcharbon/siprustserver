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
- [ ] **3. `crates/sip-message/src/generators.rs`** — 908 L, 56 direct
  refs. Do right after #1 so the two carve-ups agree on module vocabulary.
- [ ] **4. `crates/call/src/model.rs` + `crates/call/src/helpers.rs`** —
  833 + 756 L, `call` crate has 92 consumer files. `helpers.rs` is a
  grab-bag by name; bucket by type per discoverability rule 4.
- [ ] **5. `crates/sip-txn/src/layer.rs`** — 1644 L, 29 consumer crates via
  root re-export. Recent Timer-G work (f592c71) lives here — keep the RFC
  §17 citations.

### Lane 2 — hot paths & rule engines

- [ ] **6. `crates/sip-proxy/src/core/request.rs`** — 1630 L, 11 smells.
  ADR-0022 danger zone (stateless final-response contract). Sibling
  `core/response.rs` (573 L) and `core/mod.rs` (640 L) get the comment pass
  in the same sitting even though they're under/near the line bar.
- [ ] **7. `crates/b2bua/src/rules/actions.rs`** — 2295 L. The rules/
  directory is already modular; this is the oversized member. Split by
  action family; `rules/defaults.rs` (1390 L), `relay.rs` (1039 L),
  `refer_transfer.rs` (1041 L) follow in the same lane.
- [ ] **8. `crates/scenario-harness/src/actor/actor.rs` + `actor/mod.rs`**
  — 1734 + 1558 L. Treat as one job; the actor/ dir is already a module
  tree, so this is intra-directory rebalancing + comment scrub.
- [ ] **9. `crates/sip-proxy/src/load_observer.rs`** — 1157 L, 16 smells
  (highest smell density in the workspace). ELU/overload seams — the
  panic-ELU cold-start lesson means suspicious comments here get logged,
  not deleted.
- [ ] **10. `crates/b2bua/src/overload.rs`** — 1129 L, 13 smells. Pairs
  with #9 conceptually; do back-to-back.

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
6. **`agent/ua.rs::quiesce` answers EVERY queued request with a bodyless
   `200 OK`** — including an offer-carrying re-INVITE/UPDATE (RFC 3264 §5
   forbids the answerless 200) and even an ACK (which takes no response at
   all). Confined to the load driver's failed-call teardown window (those
   calls are never RFC-audited), and
   `try_receive_tolerating_blocking` exists as the assertable, compliant
   replacement — but any new use of `quiesce` on an audited path would emit
   non-compliant peer SIP. Candidate fix: skip ACKs and attach `ANSWER_SDP`
   to offer-carrying INVITEs/UPDATEs inside `quiesce` itself.
7. **`InDialogRequest::with_to_tag` doc said the shared CSeq counter still
   advances and per-fork CSeq independence is not asserted** — stale: the
   send path forks an independent per-fork CSeq sequence whenever the fork
   map is wired (`ClientInvite::send_request`). Comment rewritten to the
   actual contract during the split; no behavior change.
