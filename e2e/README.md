# E2E test management — quick how-to

End-to-end SIP tests assembled from four orthogonal axes (ADR-0018/0019):
**Callflow shape** (compiled Rust flow) × **Infra shape** (fake in-process SUT /
real sockets) × **Endpoint config** (JSON addresses) × **Test case** (JSON input
+ checks). The same shape body runs unchanged over fake and real — only
transport, clock and timeouts differ.

```
e2e/
  cases/      Test cases (input data + checks + compatible shapes)
  checksets/  Shared, reusable check bundles (referenced by id from cases)
  campaigns/  Campaigns: which cases over which infra shapes
  infra/      Endpoint configs, one per Infra shape (role → address)
  schemas/    Generated JSON Schemas ($schema in authored files → editor completion)
  runs/       Run output (gitignored): <campaign>/<ts>/<cell>/result.json + campaign.json
```

## Start the website

```sh
cargo run -p e2e-web            # http://127.0.0.1:8378/campaigns
# flags: --port <p>  --e2e-dir <dir>  --runs-root <dir>
```

List campaigns and hit **Launch**; the run page live-updates (htmx 1s poll)
until every cell settles; click a cell for the SVG call diagram + check
verdicts. `/cases/<id>` views a Test case and lets you edit it in place —
saves are validated against the compiled registries and rejected with the
precise problem list. Every route also mirrors JSON for scripts:

```sh
curl -H 'Accept: application/json' http://127.0.0.1:8378/campaigns
curl -X POST -H 'Accept: application/json' http://127.0.0.1:8378/campaigns/smoke/runs   # → {"runId": ...}
curl -H 'Accept: application/json' http://127.0.0.1:8378/runs/<runId>                   # → live status
```

## Launch a campaign headless (CI)

```sh
cargo run -p e2e-cli -- run e2e/campaigns/smoke.json
# subset:        --case <id> ... --infra <id> ...
# output root:   --runs-root <dir>   (default e2e/runs)
# run label:     --ts <label>        (default run-<unix-seconds>)
```

Exit code `0` only if **every** cell passed (failed checks → `1`, bad
input/usage → `2`) — usable directly as a CI gate. Each cell writes
`result.json` (verdicts + RFC findings + the sequence diagram as a neutral
`seqDoc`; render it with `seq_report::render_svg`); a crashed cell writes
`error.txt` instead. `campaign.json` aggregates the per-cell verdicts.

The same campaigns also run as plain tests (no CLI): `cargo test -p e2e-core`.

## Author or update a test

1. **Test case** — add/edit `e2e/cases/<id>.json` (file name = `id`), in your
   editor or on the website (`/cases/<id>`). Point `$schema` at
   `../schemas/test-case.schema.json` for completion. A case declares its
   `compatibleShapes`, the `input` (`core.from/to/ruri` + per-shape `extras`),
   shared `checkSets` it pulls in by id, and inline `checks`: blocks keyed
   `"<agent>.<anchor>"` (e.g. `bob1.initialInvite`) with field assertions —
   `from.userInfo`, `header(Max-Forwards)`, `body`, `source.ip`, … ops
   `regex|eq|exists|absent`, values may bind `${input.from}` / `${infra.lbVip}`.
2. **Check set** — reusable check bundles in `e2e/checksets/<id>.json` (e.g.
   `invite-identity`), shareable by every case whose shapes publish the anchors
   the set references.
3. **Campaign** — list case ids + infra shapes in `e2e/campaigns/<id>.json`.
4. **Lint before running** — precise load-time errors (unknown shape, anchor
   not published, missing required input):

   ```sh
   cargo run -p e2e-cli -- validate e2e/cases/<id>.json e2e/campaigns/<id>.json
   ```

Anchors and shapes are validated at load: a check can only reference an anchor
its shape publishes, so a typo fails before anything runs.

## Update the tool itself

- **New Callflow shape** (Rust): implement `CallflowShape` in
  `crates/e2e-core/src/shapes/`, tag the anchors it publishes
  (`rt.anchor("bob1", Anchor::InitialInvite, uas.request())`), register it in
  `shapes::registry()`. Keep portable shapes advance-free.
- **New Infra shape**: implement `InfraShape` in `crates/e2e-core/src/infra.rs`,
  add it to `infra::by_id`/`known_ids`, and commit its Endpoint config under
  `e2e/infra/<id>.json`.
- **Model/schema changes**: edit `crates/e2e-core/src/model.rs`, then
  regenerate the committed schemas (a CI drift test fails until you do):

  ```sh
  cargo run -p xtask -- e2e-schema      # or: cargo run -p e2e-cli -- schema
  ```

Remaining planned breadth (see
[docs/plan/e2e-test-management-website.md](../docs/plan/e2e-test-management-website.md)):
media opt-in (per-agent `.wav` + classifier check, Phase J) and the
`rerouting` / `rerouting-prack` Callflow shapes (Phase K).
