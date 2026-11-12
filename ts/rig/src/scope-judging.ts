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
 *    export's name no collector of the scope collects (`ExportJudges`), its
 *    name `not checkable` on each cell of its lane the base could not describe.
 *    A judge whose records the base could not all write does not run: its
 *    verifications are `not checkable` on every cell and the scope.
 * 5. Where the scope's input was not handed whole (an export line, file or
 *    directory skipped, `Exports.Unread`; keys or records the base could not
 *    write), every `pass` the scope's collectors and judges answered on the
 *    cell it concerns is `not checkable`, on every cell where it is the
 *    scope's, and on the scope; any other answer is kept, a `not checkable`
 *    beside it.
 * 6. A verification the scope's entries promise that nobody answered for a
 *    cell, the cell's own lines included, is `not checkable`: a collector
 *    promises those its manifest names (`answers`), or its own name where it
 *    names a judge and no `answers`; a judge of exports alone its name. A
 *    collector naming neither may answer its own name, and promises none.
 *
 * Unclaimed and ambiguous records are kept on the scope: the attachments
 * `<name>-unclaimed` and `<name>-ambiguous` (kind `log`, the JSON lines of
 * `joined/<name>.unclaimed.jsonl` and `.ambiguous.jsonl` in its directory),
 * listed with what the name's last instance answered of the scope.
 */
import type { Attachment } from "@sip/contracts"
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
import * as ScopeAnswers from "./scope-answers.js"

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
  /** The scope's cells the base could not describe: no step reads them. */
  readonly undescribed: ReadonlyArray<CollectorRun.Unread>
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

/** The files of the unclaimed and ambiguous records of `name` kept under `holder`, as a judge of `name` reads them. */
const judgingOf = (holder: string, name: string): Context.Judging => ({
  name,
  unclaimed: CellRecords.joinedFileOf(holder, `${name}.unclaimed`),
  ambiguous: CellRecords.joinedFileOf(holder, `${name}.ambiguous`)
})

/** `joined`'s unclaimed and ambiguous records of `name` written under `holder` ({@link judgingOf}), listed there where any; the attachments listed. */
const keep = (holder: string, name: string, joined: Join.Joined): ReadonlyArray<Attachment.Attachment> => {
  const files = judgingOf(holder, name)
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
  return attachments
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
  readonly judge: RigPath.Defined<JudgeManifest.Judge> | undefined
  readonly answers: ReadonlyArray<string>
  readonly scope: CollectorRun.Instance["scope"]
  readonly lane: Lanes.Loaded | undefined
  /** Why its records were not all written where its judge reads them: the judge does not run, its verifications `not checkable`. */
  readonly unwritten: string | undefined
}

/** What joining a name came to: the attachments of its unclaimed and ambiguous records, and why they or its cells' records were not written. */
interface JoinedOf {
  readonly attachments: ReadonlyArray<Attachment.Attachment>
  readonly unwritten: string | undefined
}

/** What `instance` promises: those its manifest names, or its own name where it names a judge; none where it names neither. */
const promisedBy = (instance: CollectorRun.Instance): ScopeAnswers.Promised => ({
  source: instance.collector.name,
  names: instance.judge === undefined ? instance.collector.manifest.answers ?? [] : Collector.answeredBy(instance.collector.manifest),
  covers: (cell) => instance.lanes === undefined || instance.lanes.includes(cell.lane)
})

/** `not checkable` for `name` from `name`, on each of `undescribed` on `lane` (every one where it is none), saying why it was not described. */
const undescribedAnswered = (
  name: string,
  undescribed: ReadonlyArray<CollectorRun.Unread>,
  lane: Lanes.Loaded | undefined
): ReadonlyMap<string, CollectorRun.Answers> =>
  new Map(
    undescribed
      .filter((cell) => lane === undefined || cell.lane === lane.id)
      .map((cell) => [cell.id, {
        verifications: [{ name, source: name, answer: "not-checkable" as const, message: `the cell ${cell.id} was not described: ${cell.why}` }],
        attachments: []
      }])
  )

/** The scope's collect: its collectors in order, their records joined, its judges run, its unanswered verifications filled. */
export const collect = (request: Request): Effect.Effect<ReadonlyArray<CollectorRun.Collected>> =>
  Effect.gen(function*() {
    const holder = holderOf(request)
    if (holder === undefined) return []
    const exported = yield* Effect.sync(() => Exports.read(request.cells, holder))
    const notHanded: Array<Exports.Unread> = [...exported.unread]
    yield* Effect.forEach(exported.unread, (one) => request.say(`exports: ${one.why}`), { discard: true })
    const failed = (why: string) => {
      notHanded.push({ cell: undefined, why })
      return request.say(why)
    }
    const kept = (step: string, effect: () => ReadonlyArray<Cell.Cell>, cells: ReadonlyArray<Cell.Cell>) =>
      Effect.catch(Effect.try({ try: effect, catch: errorText }), (why) => Effect.as(failed(`${step}: ${why}`), cells))
    let cells = yield* kept("exported keys", () => CellRecords.withKeys(request.cells, [...exported.byName.values()].flatMap((one) => one.keys)), request.cells)
    const collected: Array<CollectorRun.Collected> = []
    const instances: Array<CollectorRun.Instance | undefined> = []
    const promised: Array<ScopeAnswers.Promised | undefined> = []
    const joined: Array<Joined> = []
    const entry = (one: CollectorRun.Collected, instance: CollectorRun.Instance | undefined, promise: ScopeAnswers.Promised | undefined) => {
      collected.push(one)
      instances.push(instance)
      promised.push(promise)
    }
    const joinOf = (label: string, name: string, key: string | undefined, over: ReadonlyArray<Cell.Cell>, records: Iterable<Join.Entry>) =>
      Effect.gen(function*() {
        const result = Join.join(key, over, records)
        const current = cells
        const written = yield* Effect.result(Effect.try({ try: () => CellRecords.withRecords(current, name, result), catch: errorText }))
        if (Result.isSuccess(written)) cells = written.success
        const held = yield* Effect.result(Effect.try({ try: () => keep(holder, name, result), catch: errorText }))
        const unwritten = [
          ...(Result.isFailure(written) ? [`the records of ${name} not written: ${written.failure}`] : []),
          ...(Result.isFailure(held) ? [`the unclaimed and ambiguous records of ${name} not kept: ${held.failure}`] : [])
        ]
        yield* Effect.forEach(unwritten, (why) => failed(`${label}: ${why}`), { discard: true })
        const joinedOf: JoinedOf = { attachments: Result.isSuccess(held) ? held.success : [], unwritten: unwritten.length === 0 ? undefined : unwritten.join("; ") }
        return joinedOf
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
      entry({ ...one, scope: { ...one.scope, attachments: [...one.scope.attachments, ...held.attachments] } }, instance, promisedBy(instance))
      joined.push({
        name,
        cells: over.map((cell) => cell.id),
        judge: instance.judge,
        answers: Collector.answeredBy(instance.collector.manifest),
        scope: instance.scope,
        lane: instance.lane,
        unwritten: held.unwritten
      })
    }
    const collectedNames = new Set(request.steps.map((step) => step.instance.collector.name))
    for (const exportJudge of request.exportJudges?.judges ?? []) {
      const name = exportJudge.judge.name
      const exports = exported.byName.get(name)
      if (collectedNames.has(name) || exports === undefined) continue
      const held = yield* joinOf(`exports ${name}`, name, exportJudge.key, cells, exports.records)
      entry(
        {
          label: `exports ${name}`,
          scope: { verifications: [], attachments: held.attachments },
          cells: undescribedAnswered(name, request.undescribed, request.exportJudges?.lane),
          keyed: [],
          assigned: [],
          keys: []
        },
        undefined,
        { source: name, names: [name], covers: () => true }
      )
      joined.push({
        name,
        cells: cells.map((cell) => cell.id),
        judge: exportJudge.judge,
        answers: [name],
        scope: request.steps[0]?.instance.scope ?? "batch",
        lane: request.exportJudges?.lane,
        unwritten: held.unwritten
      })
    }
    const end = yield* Effect.map(Clock.currentTimeMillis, (clock_ms) => ({ clock_ms, wall_ms: Date.now() }))
    for (const one of joined) {
      if (one.judge === undefined) continue
      const ids = new Set(one.cells)
      const judged = cells.filter((cell) => ids.has(cell.id))
      const judging = {
        judge: one.judge,
        context: contextOf(one.scope, request.start, end, judged, judgingOf(holder, one.name), one.lane),
        answers: one.answers
      }
      const answered = one.unwritten === undefined
        ? yield* JudgeRun.run({ ...judging, dir: path.join(holder, JUDGES_DIR, one.name), holder })
        : JudgeRun.notCheckable(judging, `judge ${one.judge.name} not run: ${one.unwritten}`)
      entry(answered, undefined, undefined)
    }
    return ScopeAnswers.unansweredFilled(ScopeAnswers.notHandedMarked(collected, notHanded), promised, cells)
  })
