/**
 * The DELTA SHEET of a queue slice: every confrontation record of every call
 * the queue's filter keeps, folded into one line per (signature, class, rule),
 * each with a handful of distinct samples naming the call they came from.
 *
 * Read off disk per request, with no store of its own: a record lives only in
 * its cell's `confrontation.ndjson`, and the page is rare enough that reading a
 * lane whole once per visit is the price. A call whose bundle is gone, or whose
 * cell holds no confrontation file, is counted as unread, never as clean; a
 * file that does not decode is counted apart and none of its records folded.
 */
import { Calls } from "@sip/store"
import { Confrontation } from "@sip/contracts"
import * as Context from "effect/Context"
import { Effect, Layer } from "effect"
import { FileSystem } from "effect/FileSystem"
import { Path } from "effect/Path"
import { ViewerConfig } from "./config.ts"
import { queryOf, type QueueRequest, scopeOf } from "./triage.ts"

/** Distinct (captured, replayed) pairs kept per line. */
export const SAMPLES = 8

/** A sample value longer than this is cut: a body row carries the whole body. */
export const VALUE_CHARS = 400

/** Cells read at once. */
const READ_CONCURRENCY = 16

/** One record of a line, as the call it came from shows it. */
export interface DeltaSample {
  readonly capture: string
  readonly caseId: string
  /** The run directory name, as the study's `cell` parameter names it. */
  readonly session: string
  readonly cell: string
  readonly step: string
  readonly captured: readonly string[]
  readonly replayed: readonly string[]
}

/** Every record of the slice sharing one signature, class and rule. */
export interface DeltaLine {
  readonly kind: string
  readonly signature: string
  readonly klass: string
  readonly rule: string
  /** Records folded into the line. */
  readonly rows: number
  /** Cells they came from. */
  readonly cells: number
  readonly samples: readonly DeltaSample[]
}

export interface DeltaSheet {
  readonly available: boolean
  /** Why nothing, or not everything, was read. */
  readonly detail: string
  readonly lane: string
  readonly scope: string
  /** Calls the filter kept. */
  readonly matched: number
  /** Cells whose confrontation file was read. */
  readonly read: number
  /** Kept calls with no surviving bundle or no confrontation file. */
  readonly unread: number
  /** Confrontation files that did not decode; `detail` names a few. */
  readonly undecodable: number
  readonly elapsedMs: number
  /** Ordered by signature, then class, then rule. */
  readonly lines: readonly DeltaLine[]
}

export interface Interface {
  readonly sheet: (request: QueueRequest) => Effect.Effect<DeltaSheet>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Deltas") {}

const cut = (values: readonly string[]): readonly string[] =>
  values.map((value) => value.length > VALUE_CHARS ? `${value.slice(0, VALUE_CHARS)}…` : value)

/** A line while the fold runs: its samples' identities ride beside it until the sheet is served. */
interface Building {
  readonly kind: string
  readonly signature: string
  readonly klass: string
  readonly rule: string
  rows: number
  cells: number
  lastCell: string
  readonly seen: Set<string>
  readonly samples: DeltaSample[]
}

/** The fold: records of one cell at a time, so `cells` counts each cell once per line. */
export const makeFold = () => {
  const byKey = new Map<string, Building>()
  const add = (
    where: Pick<DeltaSample, "capture" | "caseId" | "session" | "cell">,
    records: ReadonlyArray<Confrontation.ConfrontationRecord>
  ): void => {
    const cellKey = `${where.session}/${where.cell}`
    for (const record of records) {
      const key = `${record.signature}\u0000${record.class}\u0000${record.rule}`
      let line = byKey.get(key)
      if (line === undefined) {
        line = {
          kind: record.kind,
          signature: record.signature,
          klass: record.class,
          rule: record.rule,
          rows: 0,
          cells: 0,
          lastCell: "",
          seen: new Set(),
          samples: []
        }
        byKey.set(key, line)
      }
      line.rows += 1
      if (line.lastCell !== cellKey) {
        line.cells += 1
        line.lastCell = cellKey
      }
      if (line.samples.length >= SAMPLES) continue
      const sampleKey = `${record.captured.join("\u0001")}\u0000${record.replayed.join("\u0001")}`
      if (line.seen.has(sampleKey)) continue
      line.seen.add(sampleKey)
      line.samples.push({
        ...where,
        step: record.step,
        captured: cut(record.captured),
        replayed: cut(record.replayed)
      })
    }
  }
  const lines = (): readonly DeltaLine[] =>
    [...byKey.values()]
      .map(({ kind, signature, klass, rule, rows, cells, samples }): DeltaLine => ({
        kind,
        signature,
        klass,
        rule,
        rows,
        cells,
        samples
      }))
      .sort((a, b) =>
        a.signature.localeCompare(b.signature) || a.klass.localeCompare(b.klass) || a.rule.localeCompare(b.rule)
      )
  return { add, lines }
}

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const fs = yield* FileSystem
    const path = yield* Path
    const config = yield* ViewerConfig.Service
    const calls = yield* Calls.Service

    const sheet = Effect.fn("Deltas.sheet")(function*(request: QueueRequest) {
      const started = Date.now()
      const scope = scopeOf(request.scope)
      const lane = request.lane ?? config.triage?.lane ?? ""
      const empty = (detail: string): DeltaSheet => ({
        available: false,
        detail,
        lane,
        scope,
        matched: 0,
        read: 0,
        unread: 0,
        undecodable: 0,
        elapsedMs: Date.now() - started,
        lines: []
      })
      if (config.triage === undefined) return empty(`no "triage" block in ${config.configPath}`)
      const listed = yield* calls.queue(lane, queryOf(request), 0).pipe(
        Effect.map((queue) => ({ queue, problem: undefined as string | undefined })),
        Effect.catch((cause) => Effect.succeed({ queue: undefined, problem: String(cause) }))
      )
      if (listed.queue === undefined) return empty(listed.problem ?? "")

      // Every case of one run shares its bundle: each directory is listed once.
      const listings = new Map<string, readonly string[]>()
      const cellsIn = Effect.fn("Deltas.cellsIn")(function*(bundle: string) {
        const known = listings.get(bundle)
        if (known !== undefined) return known
        const entries = yield* fs.readDirectory(bundle).pipe(Effect.orElseSucceed(() => [] as string[]))
        listings.set(bundle, entries)
        return entries
      })

      const fold = makeFold()
      let read = 0
      let unread = 0
      let undecodable = 0
      const problems: string[] = []
      yield* Effect.forEach(listed.queue.rows, (row) =>
        Effect.gen(function*() {
          if (row.bundlePath === null) {
            unread += 1
            return
          }
          const bundle = row.bundlePath
          const cells = (yield* cellsIn(bundle)).filter((entry) =>
            entry.startsWith(`${row.caseId}__`) && entry.endsWith(`__${lane}`)
          )
          let any = false
          for (const cell of cells) {
            const text = yield* fs.readFileString(path.join(bundle, cell, "confrontation.ndjson")).pipe(
              Effect.orElseSucceed(() => undefined)
            )
            if (text === undefined) continue
            const records = yield* Effect.try({
              try: () => Confrontation.parseConfrontationLines(text),
              catch: (cause) => String(cause)
            }).pipe(Effect.catch((problem) => {
              if (problems.length < 3) problems.push(`${cell}: ${problem}`)
              return Effect.succeed(undefined)
            }))
            any = true
            if (records === undefined) {
              undecodable += 1
              continue
            }
            read += 1
            fold.add({ capture: row.capture, caseId: row.caseId, session: path.basename(bundle), cell }, records)
          }
          if (!any) unread += 1
        }), { concurrency: READ_CONCURRENCY, discard: true })

      return {
        available: true,
        detail: problems.length === 0 ? "" : `${undecodable} undecodable confrontation file(s), e.g. ${problems.join("; ")}`,
        lane,
        scope,
        matched: listed.queue.matched,
        read,
        unread,
        undecodable,
        elapsedMs: Date.now() - started,
        lines: fold.lines()
      } satisfies DeltaSheet
    })

    return { sheet }
  })
)

export * as Deltas from "./deltas.ts"
