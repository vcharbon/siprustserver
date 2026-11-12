/**
 * A scope's collect, its records joined and judged (ADR-0060 appendix §4-§6):
 *
 * 1. The keys its exports add are added to its cells.
 * 2. Its collectors collect in order, each over its cells as the earlier ones
 *    left them: the keys a collector adds are its later collectors' to read.
 * 3. Once the last instance of a collector's name collected, the records of
 *    that name (those its instances returned, those the scope's exports hold)
 *    are joined on its manifest's `[join] key` in one pass (`join.ts`), each
 *    cell's written into its directory and named in its `records`
 *    (`cell-records.ts`); later collectors read them.
 * 4. Each judge runs once over every cell the name's instances collect: the
 *    judge its collector names, and, over its exports alone, a judge of an
 *    export's name no collector of the scope collects (`ExportJudges`).
 * 5. A verification a judged name's export left unread lines or a file of
 *    (`Exports.Unread`) is `not checkable` where the judge answered it: on the
 *    cell whose export it is, on every cell and the scope where it is the
 *    scope's.
 * 6. A verification the scope's entries promise that nobody answered for a
 *    cell, the cell's own lines included, is `not checkable`: a collector
 *    promises those its manifest names (`answers`), or its own name where it
 *    names a judge and no `answers`; a judge of exports alone its name. A
 *    collector naming neither promises none: it may only attach.
 *
 * Unclaimed and ambiguous records are kept on the scope: the attachments
 * `<name>-unclaimed` and `<name>-ambiguous` (kind `log`, the JSON lines of
 * `joined/<name>.unclaimed.jsonl` and `.ambiguous.jsonl` in its directory),
 * listed with what the name's last instance answered of the scope.
 */
import type { Attachment, Verification } from "@sip/contracts"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as path from "node:path"
import * as Attached from "./attached.js"
import type * as Cell from "./cell.js"
import * as CellRecords from "./cell-records.js"
import * as Collector from "./collector.js"
import type * as CollectorRun from "./collector-run.js"
import * as Context from "./context.js"
import * as Exports from "./exports.js"
import * as Join from "./join.js"
import type * as JudgeManifest from "./judge.js"
import * as JudgeRun from "./judge-run.js"
import type * as Lanes from "./lanes.js"
import type * as RigPath from "./rig-path.js"

/** The directory under a scope's where each judge's run keeps its files, one directory per judged name. */
export const JUDGES_DIR = "judges"

/** A judge of the path that judges a name's exports where no collector of the scope collects that name. */
export interface ExportJudge {
  readonly judge: RigPath.Defined<JudgeManifest.Judge>
  /** The cell key its records join on: the `[join] key` of the path's collector of its name, else {@link Cell.CALL_ID}. */
  readonly key: string
}

/** The judges a scope runs over its exports alone, and the scope they run in. */
export interface ExportJudges {
  readonly judges: ReadonlyArray<ExportJudge>
  /** The scope's own directory: its exports, the attachments that name no cell. */
  readonly holder: string
  /** The lane a judge's context states, where the scope is one lane's. */
  readonly lane?: Lanes.Loaded | undefined
}

/** One collector of the scope as the scope collects it. */
export interface Step {
  readonly instance: CollectorRun.Instance
  /** Its collect over the scope's cells as the earlier steps left them, those of its lanes. */
  readonly collect: (cells: ReadonlyArray<Cell.Cell>) => Effect.Effect<CollectorRun.Collected>
}

/** What a scope's collect runs with. */
export interface Request {
  readonly steps: ReadonlyArray<Step>
  readonly cells: ReadonlyArray<Cell.Cell>
  readonly exportJudges: ExportJudges | undefined
  /** When the scope opened. */
  readonly start: Cell.Instant
  /** Says a fault of the scope's exports or files: it never fails the collect. */
  readonly say: (message: string) => Effect.Effect<void>
}

/** The cells of `cells` that `instance` collects. */
const collectedBy = (instance: CollectorRun.Instance, cells: ReadonlyArray<Cell.Cell>): ReadonlyArray<Cell.Cell> =>
  instance.lanes === undefined ? cells : cells.filter((cell) => instance.lanes!.includes(cell.lane))

/** The scope's own directory. */
const holderOf = (request: Request): string | undefined => request.steps[0]?.instance.holder ?? request.exportJudges?.holder

const errorText = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/** The records of a name a scope joins: each instance's returned ones, then the exports'. */
function* recordsOf(collected: ReadonlyArray<CollectorRun.Collected>, exported: Exports.Exported | undefined): Iterable<Join.Entry> {
  for (const one of collected) {
    yield* one.keyed
    yield* one.assigned
  }
  if (exported !== undefined) yield* exported.records
}

/** `joined`'s unclaimed and ambiguous records of `name` written under `holder`, listed there where any; the attachments listed. */
const keep = (holder: string, name: string, joined: Join.Joined): { readonly files: Context.Judging; readonly attachments: ReadonlyArray<Attachment.Attachment> } => {
  const files = {
    name,
    unclaimed: CellRecords.joinedFileOf(holder, `${name}.unclaimed`),
    ambiguous: CellRecords.joinedFileOf(holder, `${name}.ambiguous`)
  }
  const attachments: Array<Attachment.Attachment> = []
  for (const [what, records, file] of [["unclaimed", joined.unclaimed, files.unclaimed], ["ambiguous", joined.ambiguous, files.ambiguous]] as const) {
    CellRecords.writeLines(file, records)
    if (records.length === 0) continue
    attachments.push(Attached.list(holder, {
      name: `${name}-${what}`,
      kind: "log",
      title: `${records.length} ${what} record(s) of ${name}`,
      file: path.relative(holder, file),
      source: name
    }))
  }
  return { files, attachments }
}

/** What a scope runs a judge with over `cells`, `judging` stated. */
const contextOf = (
  scope: CollectorRun.Instance["scope"],
  start: Cell.Instant,
  end: { readonly clock_ms: number; readonly wall_ms: number },
  cells: ReadonlyArray<Cell.Cell>,
  judging: Context.Judging,
  lane: Lanes.Loaded | undefined
): Context.Context => ({
  schema: Context.SCHEMA,
  scope,
  window: { start, end: { lane_ms: end.clock_ms, wall_ms: end.wall_ms } },
  cells,
  ...(lane === undefined ? {} : { lane: Context.laneOf(lane) }),
  params: {},
  judging
})

/** A name the scope joined, and what judges it. */
interface Joined {
  readonly name: string
  readonly cells: ReadonlyArray<string>
  readonly files: Context.Judging
  readonly judge: RigPath.Defined<JudgeManifest.Judge> | undefined
  readonly answers: ReadonlyArray<string>
  readonly scope: CollectorRun.Instance["scope"]
  readonly lane: Lanes.Loaded | undefined
  /** The name's exports not read whole. */
  readonly unread: ReadonlyArray<Exports.Unread>
}

/** What an entry of the scope's collect promises: the verifications it answers `not checkable` on the cells it covers where nobody answered them. */
interface Promised {
  readonly source: string
  readonly names: ReadonlyArray<string>
  readonly covers: (cell: Cell.Cell) => boolean
}

/** What `instance` promises: those its manifest names, or its own name where it names a judge; none where it names neither. */
const promisedBy = (instance: CollectorRun.Instance): Promised => ({
  source: instance.collector.name,
  names: instance.judge === undefined ? instance.collector.manifest.answers ?? [] : Collector.answeredBy(instance.collector.manifest),
  covers: (cell) => instance.lanes === undefined || instance.lanes.includes(cell.lane)
})

/** The names answered on each of `cells`, other than `pending`: its own lines', then `collected`'s. */
const answeredOn = (collected: ReadonlyArray<CollectorRun.Collected>, cells: ReadonlyArray<Cell.Cell>): Map<string, Set<string>> => {
  const answered = new Map(cells.map((cell) => [cell.id, new Set<string>()]))
  const add = (id: string, lines: ReadonlyArray<Pick<Verification.Verification, "name" | "answer">>) => {
    const names = answered.get(id)
    if (names === undefined) return
    for (const line of lines) if (line.answer !== "pending") names.add(line.name)
  }
  for (const cell of cells) add(cell.id, cell.verifications)
  for (const one of collected) for (const [id, held] of one.cells) add(id, held.verifications)
  return answered
}

/**
 * `collected` with, per entry's promise, each verification nobody answered
 * for one of the cells it covers answered `not checkable` by it: none of
 * `collected` answered it, nor did the cell's own lines, nor an earlier
 * entry's filling.
 */
const unansweredFilled = (
  collected: ReadonlyArray<CollectorRun.Collected>,
  promised: ReadonlyArray<Promised | undefined>,
  cells: ReadonlyArray<Cell.Cell>
): ReadonlyArray<CollectorRun.Collected> => {
  const answered = answeredOn(collected, cells)
  return collected.map((one, at) => {
    const promise = promised[at]
    if (promise === undefined || promise.names.length === 0) return one
    const byCell = new Map(one.cells)
    for (const cell of cells) {
      if (!promise.covers(cell)) continue
      const names = answered.get(cell.id)!
      const missing = promise.names.filter((name) => !names.has(name))
      if (missing.length === 0) continue
      for (const name of missing) names.add(name)
      const held = byCell.get(cell.id) ?? { verifications: [], attachments: [] }
      byCell.set(cell.id, {
        ...held,
        verifications: [
          ...held.verifications,
          ...missing.map((name) => ({
            name,
            source: promise.source,
            answer: "not-checkable" as const,
            message: `nobody answered ${name} for the cell ${cell.id}`
          }))
        ]
      })
    }
    return { ...one, cells: byCell }
  })
}

/** `answers` with each line of `names` replaced by `not checkable` from `source`, saying `whys`; as it is where there is no why. */
const unreadMarked = (answers: CollectorRun.Answers, source: string, names: ReadonlyArray<string>, whys: ReadonlyArray<string>): CollectorRun.Answers =>
  whys.length === 0 ? answers : {
    ...answers,
    verifications: [
      ...answers.verifications.filter((line) => !names.includes(line.name)),
      ...names.map((name) => ({
        name,
        source,
        answer: "not-checkable" as const,
        message: `the exports of ${name} were not read whole: ${whys.join("; ")}`
      }))
    ]
  }

/**
 * `judged`, a judge's answers over `joined`, with each of its verifications
 * `not checkable` where an export of the name was not read whole: on the cell
 * whose export it is, on every cell and on the scope where it is the scope's.
 */
const withUnread = (judged: CollectorRun.Collected, source: string, joined: Joined): CollectorRun.Collected => {
  if (joined.unread.length === 0) return judged
  const ofScope = joined.unread.flatMap((one) => (one.cell === undefined ? [one.why] : []))
  const ofCell = new Map<string, Array<string>>()
  for (const one of joined.unread) {
    if (one.cell === undefined) continue
    const whys = ofCell.get(one.cell)
    if (whys === undefined) ofCell.set(one.cell, [one.why])
    else whys.push(one.why)
  }
  return {
    ...judged,
    scope: unreadMarked(judged.scope, source, joined.answers, ofScope),
    cells: new Map([...judged.cells].map(([id, held]) => [id, unreadMarked(held, source, joined.answers, [...ofScope, ...(ofCell.get(id) ?? [])])]))
  }
}

/** The scope's collect: its collectors in order, their records joined, its judges run, its unanswered verifications filled. */
export const collect = (request: Request): Effect.Effect<ReadonlyArray<CollectorRun.Collected>> =>
  Effect.gen(function*() {
    const holder = holderOf(request)
    if (holder === undefined) return []
    const exported = yield* Effect.sync(() => Exports.read(request.cells, holder))
    yield* Effect.forEach(exported.faults, (fault) => request.say(`exports: ${fault}`), { discard: true })
    const kept = (step: string, effect: () => ReadonlyArray<Cell.Cell>, cells: ReadonlyArray<Cell.Cell>) =>
      Effect.catch(Effect.try({ try: effect, catch: errorText }), (why) => Effect.as(request.say(`${step}: ${why}`), cells))
    let cells = yield* kept("exported keys", () => CellRecords.withKeys(request.cells, [...exported.byName.values()].flatMap((one) => one.keys)), request.cells)
    const collected: Array<CollectorRun.Collected> = []
    const instances: Array<CollectorRun.Instance | undefined> = []
    const promised: Array<Promised | undefined> = []
    const joined: Array<Joined> = []
    const entry = (one: CollectorRun.Collected, instance: CollectorRun.Instance | undefined, promise: Promised | undefined) => {
      collected.push(one)
      instances.push(instance)
      promised.push(promise)
    }
    const joinOf = (label: string, name: string, key: string | undefined, over: ReadonlyArray<Cell.Cell>, records: Iterable<Join.Entry>) =>
      Effect.gen(function*() {
        const result = Join.join(key, over, records)
        const current = cells
        cells = yield* kept(`${label}: the records of ${name}`, () => CellRecords.withRecords(current, name, result), current)
        const held = yield* Effect.result(Effect.try({ try: () => keep(holder, name, result), catch: errorText }))
        if (Result.isSuccess(held)) return held.success
        yield* request.say(`${label}: the unclaimed and ambiguous records of ${name} not kept: ${held.failure}`)
        return undefined
      })
    for (const step of request.steps) {
      const one = yield* step.collect(cells)
      const before = cells
      cells = yield* kept(`${one.label}: its keys`, () => CellRecords.withKeys(before, one.keys), before)
      const { instance } = step
      const name = instance.collector.name
      const ofName = request.steps.filter((other) => other.instance.collector.name === name)
      const sameName = [...collected.filter((_, at) => instances[at]?.collector.name === name), one]
      const exports = exported.byName.get(name)
      const hasRecords = exports !== undefined || sameName.some((other) => other.keyed.length + other.assigned.length > 0)
      if (ofName.at(-1) !== step || !(hasRecords || instance.judge !== undefined)) {
        entry(one, instance, promisedBy(instance))
        continue
      }
      const over = ofName.flatMap((other) => collectedBy(other.instance, cells))
      const held = yield* joinOf(one.label, name, instance.collector.manifest.join?.key, over, recordsOf(sameName, exports))
      entry(
        held === undefined ? one : { ...one, scope: { ...one.scope, attachments: [...one.scope.attachments, ...held.attachments] } },
        instance,
        promisedBy(instance)
      )
      if (held !== undefined) {
        joined.push({
          name,
          cells: over.map((cell) => cell.id),
          files: held.files,
          judge: instance.judge,
          answers: Collector.answeredBy(instance.collector.manifest),
          scope: instance.scope,
          lane: instance.lane,
          unread: exports?.unread ?? []
        })
      }
    }
    const collectedNames = new Set(request.steps.map((step) => step.instance.collector.name))
    for (const exportJudge of request.exportJudges?.judges ?? []) {
      const name = exportJudge.judge.name
      const exports = exported.byName.get(name)
      if (collectedNames.has(name) || exports === undefined) continue
      const held = yield* joinOf(`exports ${name}`, name, exportJudge.key, cells, exports.records)
      entry(
        { label: `exports ${name}`, scope: { verifications: [], attachments: held?.attachments ?? [] }, cells: new Map(), keyed: [], assigned: [], keys: [] },
        undefined,
        { source: name, names: [name], covers: () => true }
      )
      if (held === undefined) continue
      joined.push({
        name,
        cells: cells.map((cell) => cell.id),
        files: held.files,
        judge: exportJudge.judge,
        answers: [name],
        scope: request.steps[0]?.instance.scope ?? "batch",
        lane: request.exportJudges?.lane,
        unread: exports.unread
      })
    }
    const end = yield* Effect.map(Clock.currentTimeMillis, (clock_ms) => ({ clock_ms, wall_ms: Date.now() }))
    for (const one of joined) {
      if (one.judge === undefined) continue
      const ids = new Set(one.cells)
      const judged = cells.filter((cell) => ids.has(cell.id))
      const answered = yield* JudgeRun.run({
        judge: one.judge,
        context: contextOf(one.scope, request.start, end, judged, one.files, one.lane),
        answers: one.answers,
        dir: path.join(holder, JUDGES_DIR, one.name),
        holder
      })
      entry(withUnread(answered, one.judge.name, one), undefined, undefined)
    }
    return unansweredFilled(collected, promised, cells)
  })
