# Large-file cleanup & module-split program

Goal: no source file over ~500 lines; every crate discoverable from its
`lib.rs` without reading implementation files; comments state present-tense
contracts only.

Completed entries and resolved suspicions are removed from this file — the
full record (Lane 1 + Lane 2 splits of 2026-07-15..26, suspicions #1–#13)
lives in this file's git history before the 2026-08-08 cleanup. Prior art —
copy the shape of these commits (API unchanged, `pub use` re-exports at the
old path, one commit per file): `2226086` + `f87093f` (`router.rs` →
`router/`), `b4ff15a` (`mux.rs` → `mux/`), and the `rules/actions.rs` →
`actions/` split (2026-07-25).

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
  33 such comments remain workspace-wide (rescan 2026-08-08), almost all in
  the `media`/`media-harness` crates
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
  finding — log it, don't replicate it. (The 2026-08-01 sweep drove this to
  zero in `b2bua/rules/`; keep it there.)
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

One file (or one listed group) per session for Lane 2–3 entries — the split +
consumer sweep + verification of a 1500+ line hub consumes a full context
window, and comment judgment degrades when it's shared. Smaller entries
(~500–700 L, self-contained) may be batched two or three per session.

## Order & tracking

Sizes rescanned 2026-08-08 (`find crates -name '*.rs' -not -path '*/target/*'
| xargs wc -l | awk '$1>500'`). Ranked by (fan-in × size × comment-smell),
with growth rate as a tie-breaker: a hub that is still accreting costs more
every week it waits.

### Lane 2 — hot paths & rule engines (remainder)

- [ ] **1. `crates/b2bua/src/rules/relay.rs`** — 2003 L, nearly DOUBLED
  since the 2026-07-15 scan (1039 L): fastest-growing file in the workspace
  and on the per-message hot path. Split first. Copy the `actions/` split
  shape. Satellite rule files that may absorb or donate concerns while the
  seams are open: `relay_first_18x.rs` (506), `promote_pem.rs` (573).
- [ ] **2. `crates/b2bua/src/rules/defaults.rs`** — 1445 L. `core_rules` is
  an exhaustive match on the `too_many_lines` warn ratchet — the ratchet
  stays; split the surrounding registry/config concerns.
- [ ] **3. `crates/b2bua/src/rules/refer_transfer.rs`** — 1107 L.

### Lane 3 — big but self-contained (internal fan-in only)

- [ ] **4. `crates/sip-net/src/rfc_audit/` suite** — gates EVERY test; split
  rule-family-per-file so a failing rule name maps to one file. Waiver text
  (`allow_violation` justifications) is contract, not history — keep.
  Multi-session: `rfc3261_cross.rs` (3167), `starter_peer.rs` (1895),
  `rfc3262_cross.rs` (1833), `cross_generic.rs` (1462), `rfc3264_cross.rs`
  (1043), `cseq.rs` (980), `dialog_model.rs` (964), `rfc3261_peer.rs` (915),
  `offer_answer_state.rs` (739), `rfc3264_peer.rs` (507).
- [ ] **5. `crates/failover-harness/src/harness.rs`** — 1782 L (+
  `runner.rs` 685). ha-acceptance.md danger zone.
- [ ] **6. `crates/sip-proxy-runner/src/main.rs`** — 1583 L and still
  growing (1253 at scan). Runner policy belongs in b2bua-runner-kit, not
  inline in a main.rs — moving logic out likely beats splitting in place.
- [ ] **7. `crates/sip-pcap/src/flow.rs`** — 1441 L (new arrival; the crate
  also has `bin/sipflow.rs` 569).
- [ ] **8. `crates/e2e-web/src/lib.rs`** — 1255 L.
- [ ] **9. `crates/b2bua/src/metrics.rs`** — 1198 L.
- [ ] **10. `crates/b2bua/src/decision/test_adapter.rs`** — 1175 L.
- [ ] **11. `crates/b2bua-sdk/src/model.rs`** — 1171 L.
- [ ] **12. `crates/repl-net/src/transport/simulated.rs`** — 1130 L (new
  arrival). Replication-adjacent — ha-acceptance.md danger zone.
- [ ] **13. `crates/scenario-harness/src/actor/scenarios.rs`** — 1082 L
  (queued from the actor split, entry #8 of the old tracker).
- [ ] **14. `crates/loadgen/src/report.rs`** (1008), then `driver.rs` (892),
  `app.rs` (791), `mux/retransmit.rs` (628).
- [ ] **15. `crates/b2bua/src/repl/`** — `puller.rs` (983),
  `supervisor.rs` (732), `store.rs` (535). ha-acceptance danger zone;
  comment scrub mandatory, splits only where a seam is obvious.
- [ ] **16. `crates/callshapes/src/plan.rs`** — 954 L.
- [ ] **17. `crates/media/` + `crates/media-harness/`** — the last
  TS-port-comment stronghold (most of the 33 remaining hits); only
  `transport.rs` (560) breaks 500 L, so this is chiefly a comment-scrub +
  lib.rs-TOC pass across both crates.
- [ ] **18. Remainder 500–950 L** — `b2bua-runner-kit/src/lib.rs` (924),
  `sip-proxy/src/resolver.rs` (873), `self_gate.rs` (869),
  `core/mod.rs` (772), `core/response.rs` (615),
  `core/request/route.rs` (506), `observability/metrics.rs` (719),
  `b2bua/src/store/mod.rs` (868), `router/callouts.rs` (673),
  `router/restore_hygiene.rs` (661), `router/process.rs` (647),
  `b2bua_core.rs` (655), `initial_invite.rs` (739 — ADR-0022),
  `timers.rs` (628 — module doc is load-bearing per test-clock.md, keep it),
  `rules/actions/dialog_track.rs` (502),
  `sip-message/src/parser/custom/structured_headers.rs` (866),
  `extract_fields.rs` (658), `template_match.rs` (688), `template.rs` (633),
  `header/uri.rs` (611), `sdp.rs` (581),
  `b2bua-harness/src/lib.rs` (813), `layer-harness` (see media pass),
  `scenario-harness/src/agent/dialog.rs` (664), `agent/harness.rs` (585),
  `agent/client_invite.rs` (585), `agent/server_txn.rs` (514),
  `actor/state.rs` (529), `realcall/env.rs` (580),
  `failover-harness/src/runner.rs` (685), `sip-net/src/contracts.rs` (779),
  `e2e-model/src/registry.rs` (723), `e2e-core/src/infra.rs` (606) +
  `registrar.rs` (522), `seq-report/src/lib.rs` (662) + `html.rs` (631),
  `topology/src/lib.rs` (605), `media/src/transport.rs` (560),
  `b2bua/src/decision/apply_route.rs` (513).

### Lane 4 — test files (comment scrub yes, splitting optional)

Tests don't have a public API, so the discoverability payoff is small; scrub
comments and split only when navigation genuinely hurts.

- [ ] `crates/loadgen/tests/smoke.rs` (2562), `crates/b2bua/tests/rules.rs`
  (2324), `crates/failover-harness/tests/failover.rs` (1490) +
  `call_terminate_on_backup.rs` (833) + `limiter_ha.rs` (780),
  `crates/b2bua/src/repl/real_transport_tests.rs` (1227) and the repl
  `s*_tests.rs` files (643/547/523/502),
  `crates/sip-message/tests/generators.rs` (1041),
  `crates/scenario-harness/tests/template_emission.rs` (987),
  `crates/loadgen/tests/fake_net.rs` (851),
  `crates/sip-txn/tests/fsm.rs` (818),
  `crates/call/tests/common/mod.rs` (765) + `codec_roundtrip.rs` (601),
  `crates/b2bua-harness/tests/refer_gating.rs` (624) +
  `update_matrix.rs` (501) + `proxy_b2bua.rs` + `basic_call_media.rs`
  (TS-port comments), `crates/sip-proxy/tests/health_probe_late_reply.rs`
  (540) + `load_balancer.rs` (535), the rest of the >500 L test files.

## Open notes carried forward

Loose ends from resolved suspicions that stay parked deliberately — not
work items here, just pointers so they aren't rediscovered as new findings:

- **X-Overload rides OPTIONS 200s only, never 503s** — tracked divergence
  documented beside its pin (`b2bua/src/router/responses.rs:40`,
  `options_200_stamps_x_overload_503_does_not`); revisit only when the AIMD
  rate-cap consumer lands (old suspicion #11).
- **`;em=1`/`;emerg=1` stack-identity markers are write-only**: stamped as
  an on-the-wire emergency signal, no in-tree reader consumes them (old
  suspicion #4's resolution).

## Raised suspicions log

Append entries as found (file:line, quoted comment, one-line assessment);
mark `resolved:` when closed. Entries #1–#13 (2026-07-15 .. 2026-08-01) are
all resolved and archived in this file's git history.

(none open)
