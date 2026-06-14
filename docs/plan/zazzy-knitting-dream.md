# E2E authoring UX + REFER-transfer media shape

## Context

The e2e test-management web UI (`crates/e2e-web`, ADR-0018/0019) lets you author
test-case JSON, but the editor is a plain `<textarea>` with server-side
validation only — no schema awareness while typing. Campaigns can't be edited at
all (read-only matrix preview), so their "layout" knobs (the `concurrency`
fake/real caps) are invisible. And the callflow-shape catalogue has no
call-transfer flow: the `Refer` anchor exists in the vocabulary
(`crates/e2e-core/src/shape.rs:21`) and the b2bua has a full blind-transfer state
machine (`crates/b2bua/src/rules/refer_transfer.rs`), but no e2e shape exercises
REFER end-to-end, let alone with an RTP media check after the transfer completes.

This change delivers three things the user asked for:
1. A schema-aware **Monaco** JSON editor (vendored locally) for authoring.
2. **Campaign** JSON made fully visible and editable, layout params included.
3. A new **`transfer-refer-media`** callflow shape (blind transfer via REFER with
   A↔C RTP media re-exchange + "hears" check), wired into the matrix on the fake
   and real infra shapes.

Decisions already taken with the user: Monaco is **vendored locally** (not CDN);
the transfer is a **blind transfer** (REFER, no `Replaces` — matches the b2bua's
implemented happy path); the shape runs on **fake + real** infra.

---

## Part 1 — Monaco JSON editor, schema-aware, vendored

**Goal:** replace the textarea in the case editor with Monaco, validating live
against the model-generated JSON schema.

### Vendor Monaco
- Download the `monaco-editor` minified AMD build and place its `min/vs` tree at
  `crates/e2e-web/static/vs/` (loader.js, editor/editor.main.js, the JSON worker,
  base CSS). Obtain via `npm pack monaco-editor` (or the unpkg/jsdelivr tarball),
  extract `package/min/vs` → `crates/e2e-web/static/vs`. This is the
  "vendored locally" choice — no CDN at runtime.
- Add a `.gitignore`-aware note: the `vs/` tree is checked in (it's the vendored
  dependency).

### Serve the assets + schemas from `e2e-web`
In `crates/e2e-web/src/lib.rs` (router at `router()`, lines 43-57):
- Mount the vendored dir. Add `tower-http` (feature `fs`) to
  `crates/e2e-web/Cargo.toml` and
  `.nest_service("/static", ServeDir::new(concat!(env!("CARGO_MANIFEST_DIR"), "/static")))`.
  Using `CARGO_MANIFEST_DIR` keeps it path-independent of the run cwd.
- Add `GET /schemas/{name}` → serve the **live** schema straight from
  `e2e_core::model::schemas()` (returns `Vec<(&str, Schema)>` for
  `endpoint-config`, `test-case`, `check-set`, `campaign`). Serving from the model
  (not the on-disk `e2e/schemas/`) means the editor can never validate against a
  stale schema. Match `{name}` against the tuple key; 404 otherwise.

### A reusable Monaco editor fragment
- Add a `json_editor(model_uri, schema_name, text)` Maud helper that emits a
  `<div id="editor">` + a bootstrap `<script>` that:
  - `require.config({paths:{vs:'/static/vs'}})`, sets `self.MonacoEnvironment`
    `getWorkerUrl` to a same-origin proxy importing from `/static/vs` (standard
    AMD-loader worker shim — needed so the JSON worker loads offline),
  - `require(['vs/editor/editor.main'], …)` then
    `monaco.languages.json.jsonDefaults.setDiagnosticsOptions({validate:true,
    schemas:[{uri:'/schemas/<name>', fileMatch:['<model_uri>'], schema: <fetched>}]})`,
    fetching the schema JSON from `/schemas/<name>`,
  - creates the editor on a model with URI `<model_uri>` so `fileMatch` binds.
- Replace the `case_view` textarea (`lib.rs:564-567`) with this fragment
  (`schema_name="test-case"`, `model_uri="inmemory://model/case.json"`). The
  existing `SAVE_JS` POST (`lib.rs:574-577`) changes to read
  `editor.getValue()` instead of the textarea value; keep the same
  `POST /cases/{id}` round-trip and inline "saved/rejected" result.
- Drop the `textarea` CSS rule; add minimal height/border CSS for `#editor`.

---

## Part 2 — Campaigns: full JSON visible + editable

**Goal:** campaigns get the same schema-aware editor; concurrency/layout params
shown explicitly.

In `crates/e2e-web/src/lib.rs`:
- Route: change `/campaigns/{id}` from `get(campaign_detail)` to
  `get(campaign_detail).post(campaign_save)`.
- `campaign_detail` (HTML branch, `lib.rs:192-211`): keep the matrix preview, and
  - add an explicit **Concurrency (layout)** line rendering `concurrency.fake` /
    `concurrency.real` so the knobs are visible at a glance,
  - append the `json_editor` fragment (`schema_name="campaign"`,
    `model_uri="inmemory://model/campaign.json"`) seeded with the campaign file's
    raw text (read via `std::fs::read_to_string`, mirroring `case_view`).
- Add `campaign_save` mirroring `case_save` (`lib.rs:579-604`): deserialize
  `model::Campaign` (the `deny_unknown_fields` derive already rejects typos),
  assert `campaign.id == id`, and **referentially validate** that every
  `cases[]` exists on disk under `e2e/cases/` and every `infra_shapes[]` is a
  known infra-shape id (reuse the infra registry the run path already uses — see
  `run::load_spec`), returning `422` with the list of unknown ids; then write
  pretty JSON back. This catches the common authoring mistakes before launch.

---

## Part 3 — `transfer-refer-media` callflow shape

**Goal:** a blind transfer (alice↔bob1 established → bob1 REFERs to bob2 → b2bua
drives the C-leg + NOTIFY + realign re-INVITEs → A↔bob2 media re-exchange), with a
post-transfer "hears" media check, runnable on fake + real infra.

### New shape `crates/e2e-core/src/shapes/transfer_refer_media.rs`
Model it on `shapes/basic_call_media.rs` (media open/negotiate/play/capture) and
the driving sequence in `crates/b2bua-harness/tests/refer_full_transfer.rs`
(`refer_allow_full_happy`, lines 68-129). Roles: `alice`, `bob1` (transferee /
REFER sender), `bob2` (transfer target "C").

- `id() = "transfer-refer-media"`, `media() = MediaMode::Exchange`.
- `anchors() = [InitialInvite, Answer, Ack, Refer, ReInvite, Bye]`.
- `run()` sequence:
  1. Open RTP transports for alice/bob1/bob2 on `rt.raw_network()`
     (`media::ts_endpoint`, `me.open(...)`); negotiate A↔bob1
     (`negotiate_call`, default dialog).
  2. alice INVITE bob1 `through(sut)` with cosmetic SDP; on real infra attach the
     `X-Api-Call` destination header (reuse `rt.api_call_destination()` as in
     `basic_call_media.rs:73-78`). 180/200/ACK → anchors
     `bob1.initialInvite` / `alice.answer` / `bob1.ack`.
  3. `let mut bob_dialog = bob1_uas.dialog();` then
     `bob_dialog.send_request(InDialogMethod::Refer)
        .with_header("Refer-To", <bob2 contact>)
        .with_header("X-Api-Call", <refer_key + destination=bob2 addr>)
        .send().await; refer.expect(202)`. Anchor the REFER request as
     `bob1.refer`. (Header shapes copied from `refer_full_transfer.rs`
     helpers `refer_to_charlie()` / `x_api_allow_c()`.)
  4. NOTIFY pump: `bob1.receive("NOTIFY")` (100 active) → respond 200.
  5. C-leg: `bob2.receive("INVITE")` → anchor `bob2.initialInvite`, respond 200
     with cosmetic SDP; `bob2.receive("ACK")`.
  6. NOTIFY terminated → `bob1.receive("NOTIFY")` → respond 200.
  7. c-realign re-INVITE: `bob2.receive("INVITE")` → anchor `bob2.reInvite`,
     respond 200; `bob2.receive("ACK")`.
  8. a-realign re-INVITE: `alice.receive("INVITE")` (carries bob2's SDP) → anchor
     `alice.reInvite`, respond 200; `alice.receive("ACK")`. Transfer complete.
  9. **Media re-exchange A↔bob2**: second `negotiate_call(&alice_t, &bob2_t,
     NegotiateOptions{ dialog_id: Some("a-c"), ..})` (each negotiation is
     independent — confirmed in media-harness). Play `ClipName::Alice` on alice,
     `ClipName::Bob` on bob2, `sleep(TALK_MS)`, then `rt.push_media` for
     `alice` (expects `Bob`) and `bob2` (expects `Alice`). The executor folds
     these into gating `alice.media hears Bob` / `bob2.media hears Alice`
     verdicts automatically (`e2e-core/src/media.rs` `write_and_fold`).
  10. Teardown: `alice_dialog.bye()` reaches bob2 (A↔C path);
      `bob2.receive("BYE")` respond 200; orphaned `bob1.receive("BYE")` respond
      200; `alice_bye.expect(200)`. Anchor `bob2.bye`.

  Note: the exact realign ordering and the realign SDP constants must mirror
  `refer_full_transfer.rs` — lift its `OFFER` / `ANSWER` / `*_REALIGN_*` SDP
  consts and `assert_reinvite`/leg expectations as the source of truth.

- Register in `crates/e2e-core/src/shapes/mod.rs` (add `pub mod`, `pub use`,
  and a `Box::new(TransferReferMedia)` entry in `registry()`).

### Enable REFER in the fake infra SUT
In `crates/e2e-core/src/infra.rs`, the `fake-lsbc-b2bua` shape builds its
`ScriptedDecisionEngine` (≈ lines 268-295) with `fallback` + `on_failure` (the
latter feeds the `rerouting` shape's bob2 failover). The infra is shape-agnostic,
so add `.on_refer(default_call_refer)` to that **same** builder (the hook
`route_all_with_refer` uses — see `crates/b2bua/src/decision/test_adapter.rs`),
keeping the existing failover wiring intact. Verify the builder exposes
`.on_refer(...)` alongside `.fallback`/`.on_failure`; if `route_all_with_refer`
is a non-composable shortcut, replicate its body inline. Ensure the fake endpoint
config (`e2e/infra/fake-lsbc-b2bua.json`) binds a `bob2` role (the rerouting
shape already needs it — confirm, add if missing).

### Real infra
`RealKindLb` (`infra.rs` ≈ 379-429) routes through the running kind cluster and
pins the b-leg via `X-Api-Call.destination`. The transfer shape supplies the
REFER's `X-Api-Call` (refer_key + destination=bob2) the same way. **Risk to flag
at implementation:** the real cluster's b2bua worker must honour the
REFER-authorization `X-Api-Call` mechanism; if it doesn't yet, the real cell
fails and we either gate it behind a cluster-capability note or leave the real
matrix entry documented as cluster-dependent. The fake cell is the CI-default
proof; the real cell is user-gated (matches existing real-infra policy).

### Authored docs
- `e2e/cases/transfer-refer-media.json`: `compatibleShapes:
  ["transfer-refer-media"]`, `checkSets: ["transfer-refer"]`, plus an inline
  `body` regex `m=audio` on `bob2.initialInvite`. (Media "hears" verdicts are
  auto-folded; no authored check needed for them.)
- `e2e/checksets/transfer-refer.json`: blocks asserting
  `bob1.refer` → `header(Refer-To)` exists; `alice.reInvite` → `body` regex
  `m=audio`; `bob2.initialInvite` → identity checks as desired.
- Campaign: add a focused `e2e/campaigns/transfer.json`
  (`cases:["transfer-refer-media"]`, `infraShapes:["fake-lsbc-b2bua","real-kind"]`)
  and add the case to `e2e/campaigns/full.json`.

---

## Critical files

- `crates/e2e-web/src/lib.rs` — routes, `json_editor` helper, `case_view`,
  `campaign_detail`, new `campaign_save`, `/schemas/{name}`, `/static` mount.
- `crates/e2e-web/Cargo.toml` — add `tower-http` (`fs`).
- `crates/e2e-web/static/vs/**` — vendored Monaco.
- `crates/e2e-core/src/shapes/transfer_refer_media.rs` (new) + `shapes/mod.rs`.
- `crates/e2e-core/src/infra.rs` — `.on_refer(...)` on the fake SUT engine.
- `e2e/cases/transfer-refer-media.json`, `e2e/checksets/transfer-refer.json`,
  `e2e/campaigns/transfer.json`, `e2e/campaigns/full.json`,
  `e2e/infra/fake-lsbc-b2bua.json` (bob2 role if missing).

Reused as-is: `negotiate_call`/`NegotiateOptions`
(`crates/media-harness/src/negotiate.rs`), `reference_clip`/`ClipName`/
`PlayScript`, `MediaCapture`/`write_and_fold` (`e2e-core/src/media.rs`),
`Dialog::send_request`/`InDialogMethod::Refer`/`ServerTxn::dialog`
(`crates/scenario-harness/src/agent.rs`), `default_call_refer`/
`route_all_with_refer` (`b2bua` + `b2bua-harness`), the
`refer_full_transfer.rs` driving sequence + SDP consts.

---

## Verification

1. **Build/unit:** `cargo build -p e2e-core -p e2e-web`;
   `cargo test -p e2e-core` (registry contains `transfer-refer-media`,
   `validate_case` accepts the new case/checkset, anchors resolve).
2. **Shape on fake infra:** add an `e2e-core` integration test (mirror existing
   shape tests) that runs the `transfer-refer-media` cell over `fake-lsbc-b2bua`
   under `#[tokio::test(start_paused = true)]` and asserts the cell passes,
   including the folded `alice.media hears Bob` / `bob2.media hears Alice`
   verdicts. Drive the protocol between advances per the test-clock policy in
   CLAUDE.md.
3. **Editor (manual):** `cargo run -p e2e-web` (serves `127.0.0.1:8378`); open
   `/cases/transfer-refer-media` → Monaco loads from `/static/vs`, schema errors
   underline live, "Validate & save" round-trips. Open `/campaigns/transfer` →
   matrix preview + visible concurrency + editable JSON; save with a bad
   case/shape id and confirm the `422` referential error.
4. **CLI campaign (fake):** `cargo run -p e2e-cli -- run e2e/campaigns/transfer.json`
   (or the existing run entrypoint) and confirm the fake cell passes; the real
   cell only when a kind cluster is up (user-gated).
5. No schema regen required (model types unchanged); if `e2e/schemas/` is
   refreshed for `$schema` references, run `cargo run -p xtask -- e2e-schema`.
