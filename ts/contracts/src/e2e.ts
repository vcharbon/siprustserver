/**
 * The e2e campaign records, mirroring `e2e_core::result` and
 * `e2e_model::checks`: one `RunResult` per cell of the campaign matrix, plus the
 * `campaign.json` aggregate index.
 *
 * camelCase on the wire, and a DIFFERENT byte discipline from the pivot bundle:
 * these are written with `serde_json::to_string_pretty` + `"\n"`, so keys ride
 * in DECLARATION order rather than sorted. Emit them through the `*Json`
 * builders here and `canonical.formatDeclared`, never through `canonical.format`.
 */
import * as Schema from "effect/Schema"
import { formatDeclared } from "./canonical.js"
import { Anomaly, anomalyJson, SeqDoc, seqDocJson } from "./seq.js"
import { STRICT } from "./strict.js"
import { LaneVerdict } from "./tokens.js"

/** One cell of the campaign matrix: {Test case × Callflow shape × Infra shape}. */
export const CellId = Schema.Struct({
  case: Schema.String,
  shape: Schema.String,
  infra: Schema.String
})
export interface CellId extends Schema.Schema.Type<typeof CellId> {}

/** The cell's directory name under the run dir. */
export const cellDirName = (cell: CellId): string => `${cell.case}__${cell.shape}__${cell.infra}`

/** The assertion operator of the e2e check grammar (`e2e_model::model::CheckOp`). */
export const CheckOp = Schema.Literals(["regex", "eq", "exists", "absent"])
export type CheckOp = typeof CheckOp.Type

/**
 * The outcome of one check, or of a whole block that failed/skipped at
 * resolution (then `field` is `(anchor)`).
 */
export const CheckVerdict = Schema.Struct({
  /** The block's `<agent>.<anchor>` selector. */
  on: Schema.String,
  field: Schema.String,
  op: CheckOp,
  /** The expected value after `${…}` substitution (when the op takes one). */
  expected: Schema.optionalKey(Schema.String),
  /** The extracted value; absent means the field was absent. */
  actual: Schema.optionalKey(Schema.String),
  passed: Schema.Boolean,
  detail: Schema.String
})
export interface CheckVerdict extends Schema.Schema.Type<typeof CheckVerdict> {}

/** `true` iff every verdict passed — the cell-verdict fold for the checks half. */
export const allChecksPassed = (verdicts: ReadonlyArray<CheckVerdict>): boolean => verdicts.every((v) => v.passed)

/**
 * Recorded-activity span — virtual ms under a paused clock, wall ms under a real
 * one (deliberately the recording's own timeline).
 */
export const Timings = Schema.Struct({
  firstMs: Schema.Int,
  lastMs: Schema.Int,
  messages: Schema.Int
})
export interface Timings extends Schema.Schema.Type<typeof Timings> {}

/**
 * One media artifact a cell produced: what `agent` RECEIVED, as a sibling `.wav`
 * next to `result.json` (never inlined), plus the classifier verdict.
 */
export const MediaRef = Schema.Struct({
  agent: Schema.String,
  /** The sibling file name (e.g. `alice.received.wav`), relative to the cell dir. */
  wav: Schema.String,
  /** The classifier's label for the received audio (e.g. `tone:200hz`). */
  classify: Schema.String,
  /** RMS level of the recorded PCM (silence ≈ 0). */
  rms: Schema.Number
})
export interface MediaRef extends Schema.Schema.Type<typeof MediaRef> {}

/** Everything one cell run produced. */
export const RunResult = Schema.Struct({
  cell: CellId,
  /** Checks passed AND the run's expects held AND the RFC hard gate found no gating violation. */
  passed: Schema.Boolean,
  checks: Schema.Array(CheckVerdict),
  /** Findings the report surfaces alongside the diagram, each tagged advisory/gating. */
  rfc: Schema.Array(Anomaly),
  /** Media artifacts (media-exchanging shapes only; empty otherwise). */
  media: Schema.optionalKey(Schema.Array(MediaRef)),
  seqDoc: SeqDoc,
  timings: Timings
})
export interface RunResult extends Schema.Schema.Type<typeof RunResult> {}

/** Per-cell line of the `campaign.json` aggregate. */
export const CellSummary = Schema.Struct({
  cell: CellId,
  passed: Schema.Boolean,
  /** The cell directory (relative to the run dir) holding `result.json`. */
  dir: Schema.String,
  /** Set when the cell CRASHED — there is then no `result.json`, only `error.txt`. */
  error: Schema.optionalKey(Schema.String),
  /**
   * Set when the cell was SKIPPED: the case document declares this cell's lane
   * blocked, so nothing ran and there is no bundle, only `skipped.json`. Carries
   * that lane verdict verbatim. A skipped cell is neither a pass nor a failure —
   * `passed` is false because nothing passed, and {@link campaignPassed} ignores
   * it because nothing failed either.
   */
  skipped: Schema.optionalKey(LaneVerdict)
})
export interface CellSummary extends Schema.Schema.Type<typeof CellSummary> {}

/**
 * What a skipped cell leaves at its cell root in place of a bundle, so the cell
 * directory says why on its own. The blocked lane is the cell id's `infra`.
 */
export const CellSkip = Schema.Struct({
  cell: CellId,
  /** The document's verdict for that lane, verbatim (always the `blocked:` form). */
  verdict: LaneVerdict
})
export interface CellSkip extends Schema.Schema.Type<typeof CellSkip> {}

/** The aggregate index of one campaign run. */
export const CampaignIndex = Schema.Struct({
  campaign: Schema.String,
  /** Run timestamp label (the `<ts>` path segment) — supplied by the caller. */
  ts: Schema.String,
  cells: Schema.Array(CellSummary)
})
export interface CampaignIndex extends Schema.Schema.Type<typeof CampaignIndex> {}

/** `true` iff every cell that RAN passed — a skipped cell drags no campaign down. */
export const campaignPassed = (index: CampaignIndex): boolean =>
  index.cells.every((cell) => cell.passed || cell.skipped !== undefined)

export const decodeRunResult = Schema.decodeUnknownEffect(RunResult, STRICT)
export const decodeRunResultSync = Schema.decodeUnknownSync(RunResult, STRICT)
export const decodeCampaignIndex = Schema.decodeUnknownEffect(CampaignIndex, STRICT)
export const decodeCampaignIndexSync = Schema.decodeUnknownSync(CampaignIndex, STRICT)
export const decodeCellSkip = Schema.decodeUnknownEffect(CellSkip, STRICT)
export const decodeCellSkipSync = Schema.decodeUnknownSync(CellSkip, STRICT)

// --- Declaration-order emission ----------------------------------------------

const pruned = (value: Record<string, unknown>): Record<string, unknown> =>
  Object.fromEntries(Object.entries(value).filter(([, v]) => v !== undefined))

export const cellIdJson = (cell: CellId): Record<string, unknown> => ({
  case: cell.case,
  shape: cell.shape,
  infra: cell.infra
})

export const checkVerdictJson = (verdict: CheckVerdict): Record<string, unknown> =>
  pruned({
    on: verdict.on,
    field: verdict.field,
    op: verdict.op,
    expected: verdict.expected,
    actual: verdict.actual,
    passed: verdict.passed,
    detail: verdict.detail
  })

export const mediaRefJson = (media: MediaRef): Record<string, unknown> => ({
  agent: media.agent,
  wav: media.wav,
  classify: media.classify,
  rms: media.rms
})

export const timingsJson = (timings: Timings): Record<string, unknown> => ({
  firstMs: timings.firstMs,
  lastMs: timings.lastMs,
  messages: timings.messages
})

export const runResultJson = (result: RunResult): Record<string, unknown> =>
  pruned({
    cell: cellIdJson(result.cell),
    passed: result.passed,
    checks: result.checks.map(checkVerdictJson),
    rfc: result.rfc.map(anomalyJson),
    media: result.media?.map(mediaRefJson),
    seqDoc: seqDocJson(result.seqDoc),
    timings: timingsJson(result.timings)
  })

export const cellSummaryJson = (summary: CellSummary): Record<string, unknown> =>
  pruned({
    cell: cellIdJson(summary.cell),
    passed: summary.passed,
    dir: summary.dir,
    error: summary.error,
    skipped: summary.skipped
  })

export const cellSkipJson = (skip: CellSkip): Record<string, unknown> => ({
  cell: cellIdJson(skip.cell),
  verdict: skip.verdict
})

export const campaignIndexJson = (index: CampaignIndex): Record<string, unknown> => ({
  campaign: index.campaign,
  ts: index.ts,
  cells: index.cells.map(cellSummaryJson)
})

/** `result.json` as the executor writes it: declaration order, two-space indent, trailing newline. */
export const emitRunResult = (result: RunResult): string => formatDeclared(runResultJson(result))

/** `campaign.json` as the executor writes it. */
export const emitCampaignIndex = (index: CampaignIndex): string => formatDeclared(campaignIndexJson(index))

/** `skipped.json` as the driver writes it at a skipped cell's root. */
export const emitCellSkip = (skip: CellSkip): string => formatDeclared(cellSkipJson(skip))
