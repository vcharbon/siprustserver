/**
 * The COMPARE REPORT of two run sets (`@sip/contracts` `compare-report.ts`,
 * `sip.compare/1`): one JSON object per call, and the text that renders it.
 *
 * `calls` holds the calls that moved, or every compared call where asked.
 * Within the ladder budget each call carries both replay ladders, read off its
 * cells' bundles as the text API renders them, and the reference ladder, read
 * off the store as of the AFTER cell's pinned cut; past it, or where a bundle
 * was reaped or the store holds no document, a ladder is null. How a ladder
 * is drawn is the caller's ({@link Ladders}). Every link points at the viewer
 * the caller names.
 *
 * Each call whose two replay ladders were read carries their delta
 * (`@sip/pipeline` `ladder-delta.ts`): the first event they part at once what
 * each run minted is normalised, and how many lines differ. Asked for the
 * calls whose ladders changed, the report reads every compared call's ladders
 * within the budget and lists those whose delta is not empty, whatever their
 * class.
 */
import { CaseDocuments, Compare, Cuts, Model } from "@sip/store"
import { CompareReport } from "@sip/contracts"
import { LadderDelta } from "@sip/pipeline"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as CallEvidence from "./call-evidence.js"

/** How a call's ladders are drawn, line by line. */
export interface LaddersInterface {
  /** A cell's replay ladder off what it recorded. */
  readonly replay: (cell: CallEvidence.CellRecording, title: string) => ReadonlyArray<string>
  /** The reference ladder the case document selects off a stored flows envelope; null where none draws. */
  readonly reference: (
    envelope: CallEvidence.StoredEnvelope,
    document: unknown,
    title: string
  ) => Effect.Effect<ReadonlyArray<string> | null, never, Cuts.Service>
}

export class Ladders extends Context.Service<Ladders, LaddersInterface>()("@sip/compare/Ladders") {}

export interface ReportOptions {
  /** Every compared call, unchanged ones too; the moved ones alone otherwise. */
  readonly all: boolean
  /** How many calls get their ladders read, in class order. */
  readonly ladders: number
  /** How many `capture/case` keys an `only*` list names. */
  readonly only: number
  /** The viewer every link points at, `scheme://host:port`. */
  readonly viewer: string
  /** List only the calls whose two ladders differ, whatever their class, among those within the budget. */
  readonly ladderChanged: boolean
}

const distinct = <A>(values: ReadonlyArray<A>): ReadonlyArray<A> => [...new Set(values)]

/** Each component a side's cells name, with the distinct commits they ran it on, in first-seen order. */
const componentsOf = (cells: ReadonlyArray<Compare.Cell>): ReadonlyArray<CompareReport.ComponentShas> => {
  const names = distinct(cells.flatMap((cell) => cell.code.map((component) => component.name)))
  return names.map((name) => ({
    name,
    shas: distinct(cells.map((cell) => Model.componentOf(cell.code, name)?.sha ?? null))
  }))
}

/** A side as the report states it: its selector, size, and who and what made its cells. */
export const sideJson = (side: Compare.Side): CompareReport.Side => ({
  selector: side.selector,
  runs: side.runs,
  cells: side.cells.length,
  lanes: [...distinct(side.cells.map((cell) => cell.lane))].sort(),
  envs: distinct(side.cells.map((cell) => cell.env)),
  branches: distinct(side.cells.map((cell) => cell.branch)),
  components: componentsOf(side.cells),
  laneHashes: distinct(side.cells.map((cell) => cell.laneHash)),
  sweeps: side.sweeps
})

/** Where the viewer shows one call of one run, and its text export. */
export const linksOf = (viewer: string, cell: Compare.Cell): CompareReport.Links => {
  const capture = encodeURIComponent(cell.capture)
  const query = new URLSearchParams({ lane: cell.lane, case: cell.caseId })
  const study = new URLSearchParams({
    lane: cell.lane,
    case: cell.caseId,
    cell: `run${cell.runId}/${CallEvidence.cellDirOf("", cell.caseId, cell.lane)}`
  })
  return {
    study: `${viewer}/triage/${capture}?${study.toString()}`,
    text: `${viewer}/api/triage/${capture}/runs/run${cell.runId}/text?${query.toString()}`
  }
}

/** The evidence of one cell: its ladder and known-bug rules, where its bundle is still on disk. */
const cellEvidence = Effect.fn("CompareReport.cellEvidence")(function*(cell: Compare.Cell) {
  const ladders = yield* Ladders
  const dir = CallEvidence.cellDirOf(cell.bundlePath, cell.caseId, cell.lane)
  if (!(yield* CallEvidence.cellOnDisk(dir))) return { ladder: null, knownBugs: null }
  const recording = yield* CallEvidence.readCellRecording(dir)
  return {
    ladder: ladders.replay(recording, `run${cell.runId} ${cell.caseId}`),
    knownBugs: yield* CallEvidence.knownBugs(dir)
  }
})

/** One cell as the report states it, with the evidence read off its bundle. */
export const cellJson = (
  viewer: string,
  cell: Compare.Cell,
  evidence: {
    readonly ladder: ReadonlyArray<string> | null
    readonly knownBugs: ReadonlyArray<CallEvidence.KnownBug> | null
  }
): CompareReport.Cell => ({
  run: cell.runId,
  lane: cell.lane,
  env: cell.env,
  branch: cell.branch,
  code: cell.code.map((component) => ({ name: component.name, sha: component.sha })),
  verdict: cell.verdict,
  passed: cell.passed,
  repeats: Compare.repeatsOf(cell),
  skipped: cell.skipped,
  failedStep: cell.failedStep,
  mainError: cell.mainError,
  bless: Compare.blessClass(cell),
  accepted: cell.deltas?.accepted ?? null,
  knownBugs: evidence.knownBugs,
  ladder: evidence.ladder,
  links: linksOf(viewer, cell)
})

/** A cell whose bundle is not read: no ladder, no known-bug rules. */
export const NO_EVIDENCE = { ladder: null, knownBugs: null } as const

/** The delta of a call's two replay ladders; null where either was not read. */
export const deltaOf = (
  before: ReadonlyArray<string> | null,
  after: ReadonlyArray<string> | null
): CompareReport.LadderDelta | null => (before === null || after === null ? null : LadderDelta.ladderDelta(before, after))

const onlyOf = (alone: Compare.Elsewhere, shown: number): CompareReport.Only => ({
  captures: alone.captures,
  count: alone.calls,
  calls: alone.keys.slice(0, shown)
})

/** The comparison as its report document. */
export const document = Effect.fn("CompareReport.document")(function*(
  comparison: Compare.Comparison,
  options: ReportOptions
) {
  const store = yield* CaseDocuments.Service
  const candidates = options.all || options.ladderChanged
    ? comparison.moves
    : comparison.moves.filter((move) => move.class !== "unchanged")
  const drawn = yield* Effect.forEach(candidates, (move, at) =>
    Effect.gen(function*() {
      const within = at < options.ladders
      const before = within ? yield* cellEvidence(move.before) : NO_EVIDENCE
      const after = within ? yield* cellEvidence(move.after) : NO_EVIDENCE
      return { move, within, before, after, delta: deltaOf(before.ladder, after.ladder) }
    }))
  const listed = options.ladderChanged
    ? drawn.filter((one) => one.delta !== null && one.delta.differingLines > 0)
    : drawn
  const read = listed.filter((one) => one.within).map((one) => one.move)
  const documents = yield* store.documents(
    read.flatMap((move) => (move.after.documentHash === null ? [] : [move.after.documentHash]))
  )
  const readings = read.flatMap((move) =>
    move.after.prepId === null ? [] : [{ capture: move.capture, prepId: move.after.prepId }]
  )
  const flows = yield* store.flowsOf(readings)
  const envelopes = new Map<string, CallEvidence.StoredEnvelope | null>()
  const envelopeOf = (hash: string) =>
    envelopes.has(hash)
      ? Effect.succeed(envelopes.get(hash)!)
      : Effect.tap(CallEvidence.storedEnvelope(hash), (envelope) => Effect.sync(() => envelopes.set(hash, envelope)))

  const refOf = Effect.fn("CompareReport.refOf")(function*(move: Compare.Move) {
    const doc = move.after.documentHash === null ? undefined : documents.get(move.after.documentHash)
    const hash = move.after.prepId === null ? undefined : flows.get(CaseDocuments.readingKey(move.capture, move.after.prepId))
    if (doc === undefined || hash === undefined) return null
    const envelope = yield* envelopeOf(hash)
    if (envelope === null) return null
    return yield* (yield* Ladders).reference(envelope, doc, `${move.capture} ${move.caseId}`)
  })

  const calls = yield* Effect.forEach(listed, ({ move, within, before, after, delta }) =>
    Effect.gen(function*() {
      return {
        alias: move.caseId,
        capture: move.capture,
        caseId: move.caseId,
        class: move.class,
        document: move.document,
        code: move.code,
        lane: move.lane,
        before: cellJson(options.viewer, move.before, before),
        after: cellJson(options.viewer, move.after, after),
        ref: within ? yield* refOf(move) : null,
        delta
      } satisfies CompareReport.Call
    }))

  return {
    schema_version: CompareReport.SCHEMA_VERSION,
    headline: Compare.headline(comparison),
    code: Compare.overallCode(comparison),
    compared: comparison.compared,
    unexplained: Compare.unexplained(comparison),
    before: sideJson(comparison.before),
    after: sideJson(comparison.after),
    counts: comparison.counts,
    onlyBefore: onlyOf(comparison.onlyBefore, options.only),
    onlyAfter: onlyOf(comparison.onlyAfter, options.only),
    calls
  } satisfies CompareReport.CompareReport
})

const list = (values: ReadonlyArray<string | null>): string =>
  values.length === 0 ? "-" : values.map((value) => value ?? "unknown").join(", ")

const short = (sha: string | null): string | null => (sha === null ? null : sha.slice(0, 10))

const sideLine = (label: string, side: CompareReport.Side): string =>
  `  ${label}: ${side.selector} — ${side.runs} run(s), ${side.cells} call(s); env ${list(side.envs)}; ` +
  `branch ${list(side.branches)}; ` +
  (side.components.length === 0
    ? "code unknown; "
    : side.components.map((component) => `${component.name} ${list(component.shas.map(short))}; `).join("")) +
  `lane ${list(side.lanes)}` +
  (side.sweeps.length === 0
    ? ""
    : `; sweeps ${side.sweeps.map((sweep) => `${sweep.id} (${sweep.selection ?? `--${sweep.universe}`})`).join(", ")}`)

/** What a cell's outcome is read off, in the store's shape or the report's. */
export interface Outcome {
  readonly passed: boolean
  readonly verdict: string
  readonly skipped: string | null
  readonly failedStep: string | null
  readonly mainError: CompareReport.Cell["mainError"]
}

/**
 * A cell's main error as ` · <kind> <rule> at <step> waiting <expected>`, and
 * the source fault beside it as ` · beside it source-fault <rule>`; empty where
 * it has none.
 */
export const errorLine = (cell: Outcome): string => {
  const error = cell.mainError
  if (error === null) return ""
  return ` · ${error.kind}${error.rule === null ? "" : ` ${error.rule}`}` +
    (error.step === null ? "" : ` at ${error.step}`) +
    (error.expected === null ? "" : ` waiting ${error.expected}`) +
    (error.source === undefined ? "" : ` · beside it source-fault ${error.source.rule}`)
}

/** How a cell ended: a pass the verdict did not give says which verdict it restated. */
export const outcomeOf = (cell: Outcome): string => {
  if (cell.skipped !== null) return `skipped ${cell.skipped}`
  const at = cell.failedStep === null ? "" : ` at ${cell.failedStep}`
  if (!cell.passed) return `failed${at}`
  return cell.verdict === "ok" || cell.verdict === "ok-negative" ? "passed" : `passed · verdict ${cell.verdict}${at}`
}

/** Repeats that are more than the cell itself, as ` · repeats P/N passed`. */
const repeatsLine = (cell: CompareReport.Cell): string =>
  cell.repeats.runs <= 1 ? "" : ` · repeats ${cell.repeats.passed}/${cell.repeats.runs} passed`

/** One cell on one line: its run, who made it, how it ended, its main error, repeats and bless class. */
export const cellLine = (label: string, cell: CompareReport.Cell): string =>
  `  ${label} run${cell.run} (${cell.env ?? "unknown"}@${cell.branch ?? "unknown"}) ${outcomeOf(cell)}` +
  errorLine(cell) + repeatsLine(cell) +
  ` · ${cell.bless}${cell.accepted === null ? "" : ` · accepted ${cell.accepted}`}` +
  (cell.knownBugs === null || cell.knownBugs.length === 0
    ? ""
    : ` · known-bug ${cell.knownBugs.map((bug) => `${bug.rule}${bug.ticket === null ? "" : ` (${bug.ticket})`}`).join(", ")}`)

/** A call's ladder delta: none, not read, or how many lines differ and the two lines it parts at first. */
export const deltaLines = (delta: CompareReport.LadderDelta | null): ReadonlyArray<string> => {
  if (delta === null) return ["  ladder delta: not read"]
  const first = delta.firstDifference
  if (first === null) return ["  ladder delta: none"]
  return [
    `  ladder delta: ${delta.differingLines} line(s) differ; first at m${first.line}`,
    `    before ${first.before ?? "(ladder ends)"}`,
    `    after  ${first.after ?? "(ladder ends)"}`
  ]
}

const ladder = (title: string, lines: ReadonlyArray<string> | null): ReadonlyArray<string> =>
  lines === null ? [`  -- ${title}: not read`] : [`  -- ${title}`, ...lines.map((line) => `    ${line}`)]

/** The document as text: the headline, each side, the counts, then each listed call, at most `limit`. */
export const render = (doc: CompareReport.CompareReport, limit: number): string => {
  const counts = Compare.MOVE_CLASSES
    .filter((one) => one !== "unchanged" || doc.counts[one].sameDocument + doc.counts[one].documentChanged > 0)
    .map((one) => {
      const count = doc.counts[one]
      return `  ${one.padEnd(16)} ${count.sameDocument + count.documentChanged}` +
        ` (same document ${count.sameDocument}, document changed ${count.documentChanged})`
    })
  const calls = doc.calls.slice(0, limit).flatMap((call) => [
    "",
    `== ${call.class}  ${call.capture}  ${call.caseId}  [document ${call.document} · code ${call.code} · ` +
    `lane ${call.lane}]`,
    cellLine("before", call.before),
    cellLine("after ", call.after),
    ...deltaLines(call.delta),
    `  study ${call.after.links.study}`,
    `  text  ${call.before.links.text}`,
    `        ${call.after.links.text}`,
    ...ladder("before ladder", call.before.ladder),
    ...ladder("after ladder", call.after.ladder),
    ...ladder("reference ladder", call.ref)
  ])
  return [
    `compare ${doc.before.selector} → ${doc.after.selector}: ${doc.headline}`,
    sideLine("before", doc.before),
    sideLine("after ", doc.after),
    `  calls: ${doc.compared} both ran; within the captures both ran, ${doc.onlyBefore.count} only before and ` +
    `${doc.onlyAfter.count} only after; captures ${doc.onlyBefore.captures} only before, ` +
    `${doc.onlyAfter.captures} only after`,
    ...counts,
    ...calls,
    ...(doc.calls.length > limit ? ["", `… ${doc.calls.length - limit} more call(s) in the JSON (--json)`] : [])
  ].join("\n")
}
