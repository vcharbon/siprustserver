/**
 * The machine-readable **load run index** (`load-result.json`), mirroring
 * `e2e_model::loadrun`: one loadgen run's result as a single authored-shape
 * document, so the e2e website renders a load run beside a functional campaign
 * run without re-deriving anything.
 *
 * Every path in it is RELATIVE to the run directory, so a run dir is
 * self-contained and portable. camelCase on the wire, declaration-order bytes —
 * the same discipline as the campaign records.
 */
import * as Schema from "effect/Schema"
import { formatDeclared } from "./canonical.js"
import { defaulted } from "./serde.js"
import { STRICT } from "./strict.js"

/** Run metadata: timing and the echoed run knobs. */
export const LoadRunMeta = Schema.Struct({
  /** Wall-clock start, Unix epoch milliseconds. */
  startedMs: Schema.Int,
  /** Wall-clock finish (the moment this doc was written), Unix epoch ms. */
  finishedMs: Schema.Int,
  /** `false` while a periodic snapshot is written mid-run; `true` at run end. */
  finished: Schema.Boolean,
  /** The SUT ingress the INVITEs were routed through (the `lb`/VIP address). */
  target: Schema.String,
  /** Offered call rate the run was configured for (calls/s). */
  cps: Schema.Number,
  durationSecs: Schema.Int,
  /** Max concurrent in-flight calls (offered load above this was shed). */
  maxInFlight: Schema.Int,
  /** The egress policy label the run realized its b-leg with. */
  egress: Schema.optionalKey(Schema.String),
  profile: Schema.optionalKey(Schema.String)
})
export interface LoadRunMeta extends Schema.Schema.Type<typeof LoadRunMeta> {}

/** One `(scenario, class, case, chaos)` completed-call count. */
export const CountRow = Schema.Struct({
  scenario: Schema.String,
  /** The result class label (`ok`, `timeout`, `status_486`, `check_fail`, …). */
  class: Schema.String,
  /** The bounded case discriminator refining the class. Empty = un-refined. */
  case: defaulted(Schema.String, ""),
  /** Chaos proximity: `clear` (genuine) or `near` (accepted kill collateral). */
  chaos: Schema.String,
  count: Schema.Int,
  /** `true` iff `class == "ok"`. */
  ok: Schema.Boolean
})
export interface CountRow extends Schema.Schema.Type<typeof CountRow> {}

/** Per-scenario end-to-end latency summary (milliseconds). */
export const LatencyRow = Schema.Struct({
  scenario: Schema.String,
  n: Schema.Int,
  meanMs: Schema.Number,
  p50Ms: Schema.Number,
  p90Ms: Schema.Number,
  p99Ms: Schema.Number,
  maxMs: Schema.Number
})
export interface LatencyRow extends Schema.Schema.Type<typeof LatencyRow> {}

/** Per-`(scenario, checkpoint)` named-checkpoint latency summary (milliseconds). */
export const CheckpointRow = Schema.Struct({
  scenario: Schema.String,
  checkpoint: Schema.String,
  n: Schema.Int,
  p50Ms: Schema.Number,
  p90Ms: Schema.Number,
  p99Ms: Schema.Number
})
export interface CheckpointRow extends Schema.Schema.Type<typeof CheckpointRow> {}

/** Per-scenario check-verdict tally over the sampled calls. */
export const CheckSummaryRow = Schema.Struct({
  scenario: Schema.String,
  passed: Schema.Int,
  failed: Schema.Int
})
export interface CheckSummaryRow extends Schema.Schema.Type<typeof CheckSummaryRow> {}

/** The run's cross-call health canaries — the first things a triager scans. */
export const Canaries = Schema.Struct({
  /** Inbound datagrams that matched no live call; should be ~0 in a clean run. */
  orphans: Schema.Int,
  /** Offered calls dropped at the max-in-flight cap. */
  shed: Schema.Int,
  /** Datagrams discarded by the simulated packet-loss model. */
  drops: Schema.Int,
  /** Calls that reached the ring→answer step (the 18x-delivery denominator). */
  ringingExpected: Schema.Int,
  /** Of those, how many saw the 18x ringing provisional. A RATE gated at >99%. */
  ringingReceived: Schema.Int
})
export interface Canaries extends Schema.Schema.Type<typeof Canaries> {}

/** The 18x ringing-delivery ratio in `[0,1]` (`1.0` when nothing rang). */
export const ringingRatio = (canaries: Canaries): number =>
  canaries.ringingExpected === 0 ? 1 : canaries.ringingReceived / canaries.ringingExpected

/** The stored sampled callflow pages for one `(scenario, class, case, chaos)` bucket. */
export const SampleGroup = Schema.Struct({
  scenario: Schema.String,
  class: Schema.String,
  case: defaulted(Schema.String, ""),
  chaos: Schema.String,
  /** Run-dir-relative paths to each sample's rendered callflow HTML page. */
  pages: Schema.Array(Schema.String)
})
export interface SampleGroup extends Schema.Schema.Type<typeof SampleGroup> {}

/** One load run's complete result, as persisted to `load-result.json`. */
export const LoadRunIndex = Schema.Struct({
  meta: LoadRunMeta,
  counts: Schema.Array(CountRow),
  latency: Schema.Array(LatencyRow),
  checkpoints: Schema.optionalKey(Schema.Array(CheckpointRow)),
  checks: Schema.optionalKey(Schema.Array(CheckSummaryRow)),
  canaries: Canaries,
  samples: Schema.optionalKey(Schema.Array(SampleGroup))
})
export interface LoadRunIndex extends Schema.Schema.Type<typeof LoadRunIndex> {}

export const decodeLoadRunIndex = Schema.decodeUnknownEffect(LoadRunIndex, STRICT)
export const decodeLoadRunIndexSync = Schema.decodeUnknownSync(LoadRunIndex, STRICT)

/** Total completed calls across every bucket. */
export const totalCalls = (index: LoadRunIndex): number => index.counts.reduce((sum, row) => sum + row.count, 0)

/** Completed calls that were NOT `ok`. */
export const failedCalls = (index: LoadRunIndex): number =>
  index.counts.filter((row) => !row.ok).reduce((sum, row) => sum + row.count, 0)

/** Genuine (chaos=clear) non-ok calls — the triage total that excludes kill collateral. */
export const clearFailures = (index: LoadRunIndex): number =>
  index.counts.filter((row) => !row.ok && row.chaos === "clear").reduce((sum, row) => sum + row.count, 0)

// --- Declaration-order emission ----------------------------------------------

const pruned = (value: Record<string, unknown>): Record<string, unknown> =>
  Object.fromEntries(Object.entries(value).filter(([, v]) => v !== undefined))

export const loadRunIndexJson = (index: LoadRunIndex): Record<string, unknown> =>
  pruned({
    meta: pruned({
      startedMs: index.meta.startedMs,
      finishedMs: index.meta.finishedMs,
      finished: index.meta.finished,
      target: index.meta.target,
      cps: index.meta.cps,
      durationSecs: index.meta.durationSecs,
      maxInFlight: index.meta.maxInFlight,
      egress: index.meta.egress,
      profile: index.meta.profile
    }),
    counts: index.counts.map((row) => ({
      scenario: row.scenario,
      class: row.class,
      case: row.case,
      chaos: row.chaos,
      count: row.count,
      ok: row.ok
    })),
    latency: index.latency.map((row) => ({
      scenario: row.scenario,
      n: row.n,
      meanMs: row.meanMs,
      p50Ms: row.p50Ms,
      p90Ms: row.p90Ms,
      p99Ms: row.p99Ms,
      maxMs: row.maxMs
    })),
    checkpoints: index.checkpoints?.map((row) => ({
      scenario: row.scenario,
      checkpoint: row.checkpoint,
      n: row.n,
      p50Ms: row.p50Ms,
      p90Ms: row.p90Ms,
      p99Ms: row.p99Ms
    })),
    checks: index.checks?.map((row) => ({ scenario: row.scenario, passed: row.passed, failed: row.failed })),
    canaries: {
      orphans: index.canaries.orphans,
      shed: index.canaries.shed,
      drops: index.canaries.drops,
      ringingExpected: index.canaries.ringingExpected,
      ringingReceived: index.canaries.ringingReceived
    },
    samples: index.samples?.map((group) => ({
      scenario: group.scenario,
      class: group.class,
      case: group.case,
      chaos: group.chaos,
      pages: group.pages
    }))
  })

export const emitLoadRunIndex = (index: LoadRunIndex): string => formatDeclared(loadRunIndexJson(index))
