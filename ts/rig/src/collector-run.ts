/**
 * The run of a scope's collectors (ADR-0060 appendix §3, §5): each opened
 * before its scope, collected over the scope's cells once it ended, and
 * closed after it, whatever ended it: a failure, an interruption, a signal
 * to the runner. A scope's named box-wide locks, the union of its
 * instances', are taken once before its first `open` and given back after
 * its last `close`; where they are refused, an instance naming one runs none
 * of its steps. An `on-failure` instance opens and closes as any other, and
 * collects over the scope's failed cells alone, not at all where none failed.
 *
 * A failed or timed-out `open` or `collect` answers `not checkable`, with
 * what failed, for every verification its manifest names
 * (`Collector.answeredBy`) on every cell of its scope and on the scope
 * itself. A `close` that fails is run once more; a second failure is the
 * event `lab-left-changed`, the job's mark. The scope's collect, its records
 * joined and judged, is `scope-judging.ts`'s.
 */
import { Attachment, Verification } from "@sip/contracts"
import * as Cause from "effect/Cause"
import * as Clock from "effect/Clock"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Result from "effect/Result"
import type * as Scope from "effect/Scope"
import * as fs from "node:fs"
import * as path from "node:path"
import type * as Cell from "./cell.js"
import * as Collector from "./collector.js"
import * as Attached from "./attached.js"
import * as Lines from "./collector-lines.js"
import * as Context from "./context.js"
import * as Executable from "./executable.js"
import type * as Judge from "./judge.js"
import type * as Lanes from "./lanes.js"
import type * as RigPath from "./rig-path.js"
import * as ScopeJudging from "./scope-judging.js"

/** What an event says of a step; `refused` is a pre-check's `fail`, which refuses its scope (`pre-check.ts`). */
export const PHASES = ["open", "opened", "collect", "collected", "close", "closed", "failed", "lab-left-changed", "refused"] as const
export type Phase = (typeof PHASES)[number]

/** The phase of the event that marks a job whose lab a collector's `close` left changed. */
export const LAB_LEFT_CHANGED: Phase = "lab-left-changed"

/** How many times `close` runs before the lab is said left changed. */
export const CLOSE_ATTEMPTS = 2

/** The directory under a holder's where an instance's attached files go, one directory per instance. */
export const COLLECTED_DIR = Attached.COLLECTED_DIR

/** One phase of one collector instance, at its instant on the runner's clock and on the wall clock. */
export interface Event {
  /** The instance's label. */
  readonly collector: string
  readonly phase: Phase
  /** Milliseconds on the runner's clock. */
  readonly clock_ms: number
  /** Unix milliseconds. */
  readonly wall_ms: number
  /** What failed, and the log that says more; absent on a positive answer. */
  readonly message?: string
}

/** Where the events of a run go: a job's record, a log. It never fails the run. */
export type Sink = (event: Event) => Effect.Effect<void>

/**
 * Takes the box-wide locks `names` for `holder`, all or none: the effect that
 * gives them back, or who holds one of them. Interrupted while it waits, it
 * holds none.
 */
export type Locker = (names: ReadonlyArray<string>, holder: string) => Effect.Effect<Effect.Effect<void>, string>

/** One collector as it runs for one scope. */
export interface Instance {
  readonly collector: RigPath.Defined<Collector.Collector>
  /** File-safe and unique among its scope's instances: its directory's name, and its attachments' default name. */
  readonly id: string
  /** What its events name it by. */
  readonly label: string
  readonly scope: "job" | "batch"
  /** The lane its context states, whose parts it reads; none for a job-scope collector that needs no part. */
  readonly lane: Lanes.Loaded | undefined
  /** The lanes whose cells it collects; every cell of its scope where absent. */
  readonly lanes: ReadonlyArray<string> | undefined
  /** Its manifest's parameters after the lane's overrides. */
  readonly params: Readonly<Record<string, unknown>>
  /** Its own directory: `state/` and `out/`, its context file, a log per step. */
  readonly dir: string
  /** The scope's own directory, which holds the attachments that name no cell. */
  readonly holder: string
  /** The judge its manifest names, as the path defines it; none judges its records where absent. */
  readonly judge?: RigPath.Defined<Judge.Judge> | undefined
  /** An `on-demand` collector its scope asked for by name; a pre-check so asked never fails open (`pre-check.ts`). */
  readonly asked?: true
  /**
   * Set `on-failure`: its `collect` reads only the cells with a failed line by
   * then ({@link failedOf}), and does not run where none failed.
   */
  readonly onFailure?: boolean
}

/** What one holder was answered: its verification lines, the last per name, and its attachments, already listed in its directory. */
export interface Answers {
  readonly verifications: ReadonlyArray<Verification.Verification>
  readonly attachments: ReadonlyArray<Attachment.Attachment>
}

/** What one instance's `collect` came to, or one judge's run, or a name's exports judged alone (`scope-judging.ts`). */
export interface Collected {
  /** The instance's label; `judge <judge> of <name>` for a judge's run; `exports <name>` for exports judged alone. */
  readonly label: string
  /** The scope's own answers. */
  readonly scope: Answers
  /** Per cell of the instance's scope, its answers, empty where it got none. */
  readonly cells: ReadonlyMap<string, Answers>
  /** The records to join by the manifest's key. */
  readonly keyed: ReadonlyArray<Lines.KeyedRecord>
  /** The records the collector assigned to a cell itself. */
  readonly assigned: ReadonlyArray<Lines.AssignedRecord>
  /** The keys it added to cells. */
  readonly keys: ReadonlyArray<Lines.KeyLine>
}

/** A cell of the scope the base could not describe as `cell.v1`: no collector reads it. */
export interface Unread {
  readonly id: string
  readonly lane: string
  /** Why it has no description. */
  readonly why: string
}

/** A scope's cells as its collectors read them: those described, and those the base could not describe. */
export interface Over {
  readonly cells: ReadonlyArray<Cell.Cell>
  readonly unread: ReadonlyArray<Unread>
}

/** A scope's opened collectors. */
export interface Opened {
  /**
   * Collects every instance in order over `cells`, the scope's cells, each
   * instance over those of its lanes as the earlier ones left them, then joins
   * and judges their records (`scope-judging.ts`): what each instance, then
   * each judge, answered. Called once, when the scope ended. Each cell of
   * `unread` on an instance's lanes is answered `not checkable` by it, and by
   * each judge of exports alone, saying why the cell was not described.
   */
  readonly collect: (cells: ReadonlyArray<Cell.Cell>, unread?: ReadonlyArray<Unread>) => Effect.Effect<ReadonlyArray<Collected>>
  /** The labels of the instances whose `close` left the lab changed so far, in order. */
  readonly leftChanged: () => ReadonlyArray<string>
}

/** An instance as its scope runs it: when it opened, and what kept it from opening. */
interface Opening {
  readonly instance: Instance
  start: Context.Context["window"]["start"]
  /** Set once it went past its scope's locks; its `close` runs then. */
  closes: boolean
  /** Why it did not open; its `collect` does not run and its verifications are not checkable. */
  failure: string | undefined
}

/** The locks `instance`'s manifest names. */
const locksOf = (instance: Instance): ReadonlyArray<string> => instance.collector.manifest.locks ?? []

/** The locks of a scope of `instances`: the union of theirs, sorted, and the labels of those naming one as their holder. */
const scopeLocksOf =(instances: ReadonlyArray<Instance>) => ({
  names: [...new Set(instances.flatMap(locksOf))].sort(),
  holder: instances.filter((instance) => locksOf(instance).length > 0).map((instance) => instance.label).join(", ")
})

const layoutOf = (instance: Instance) => ({
  state: path.join(instance.dir, "state"),
  out: path.join(instance.dir, "out"),
  context: path.join(instance.dir, "context.json")
})

const now = Effect.map(Clock.currentTimeMillis, (clock_ms) => ({ clock_ms, wall_ms: Date.now() }))

const emit = (sink: Sink, instance: Instance, phase: Phase, message?: string) =>
  Effect.flatMap(now, (at) => sink({ collector: instance.label, phase, ...at, ...(message === undefined ? {} : { message }) }))

const errorText = (cause: unknown): string => (cause instanceof Error ? cause.message : String(cause))

/** `context` written as `instance`'s context file. */
const writeContext = (instance: Instance, context: Omit<Context.Context, "schema" | "scope" | "lane" | "params">) => {
  const written: Context.Context = {
    schema: Context.SCHEMA,
    scope: instance.scope,
    ...context,
    ...(instance.lane === undefined ? {} : { lane: Context.laneOf(instance.lane) }),
    params: { ...instance.params }
  }
  fs.writeFileSync(layoutOf(instance).context, `${JSON.stringify(written, null, 2)}\n`)
}

/** `instance`'s directory emptied and laid out, its context written with no cell and its window opening at `start`. */
const layOut = (instance: Instance, start: Context.Context["window"]["start"]) =>
  Effect.try({
    try: () => {
      const layout = layoutOf(instance)
      fs.rmSync(instance.dir, { recursive: true, force: true })
      fs.mkdirSync(layout.state, { recursive: true })
      fs.mkdirSync(layout.out, { recursive: true })
      writeContext(instance, { window: { start }, cells: [] })
    },
    catch: (cause) => `its directory ${instance.dir} not laid out: ${errorText(cause)}`
  })

/** The last line of `file` that says something, at most 300 characters of it; `undefined` where none does. */
const lastLineOf = (file: string): string | undefined => {
  try {
    const size = fs.statSync(file).size
    const fd = fs.openSync(file, "r")
    const buffer = Buffer.alloc(Math.min(size, 4096))
    fs.readSync(fd, buffer, 0, buffer.length, size - buffer.length)
    fs.closeSync(fd)
    const last = buffer.toString("utf8").split("\n").map((line) => line.trim()).filter((line) => line !== "").at(-1)
    return last === undefined ? undefined : last.slice(0, 300)
  } catch {
    return undefined
  }
}

/** Runs `step`'s executable `file` of `instance` within `timeoutMs`; how it ended and what that says, its log's last line included. */
const runStep = (instance: Instance, file: string, step: "open" | "collect" | "close", timeoutMs: number, stdout?: string) => {
  const log = path.join(instance.dir, `${step}.log`)
  return Effect.map(
    Executable.run({
      dir: instance.collector.dir,
      file,
      ...layoutOf(instance),
      step,
      timeoutMs,
      log,
      ...(stdout === undefined ? {} : { stdout })
    }),
    (ended) => {
      const last = Executable.succeeded(ended) ? undefined : lastLineOf(log)
      return { ended, said: `${step} ${Executable.describe(ended)}${last === undefined ? "" : `: ${last}`}, its log ${log}` }
    }
  )
}

/**
 * Runs `opening`'s `open`, `opening.closes` set once it went past its
 * scope's locks: `refused`, why they were not taken, where they were not
 * and it names one.
 */
const openOne = (
  opening: Opening,
  refused: string | undefined,
  options: Options,
  restore: <A, E, R>(effect: Effect.Effect<A, E, R>) => Effect.Effect<A, E, R>
) =>
  Effect.gen(function*() {
    const { instance } = opening
    const manifest = instance.collector.manifest
    if (refused !== undefined && locksOf(instance).length > 0) {
      opening.failure = `its locks not taken: ${refused}`
      yield* emit(options.sink, instance, "failed", opening.failure)
      return
    }
    opening.closes = true
    const laid = yield* Effect.result(layOut(instance, opening.start))
    if (Result.isFailure(laid)) {
      opening.failure = laid.failure
      yield* emit(options.sink, instance, "failed", `open not run: ${laid.failure}`)
      return
    }
    const file = manifest.run.open
    if (file === undefined) return
    yield* emit(options.sink, instance, "open")
    const ran = yield* restore(runStep(instance, file, "open", Collector.boundsOf(manifest).open_s * 1000)).pipe(
      Effect.onInterrupt(() => emit(options.sink, instance, "failed", "open interrupted, its process group killed"))
    )
    if (!Executable.succeeded(ran.ended)) {
      opening.failure = ran.said
      yield* emit(options.sink, instance, "failed", ran.said)
      return
    }
    yield* emit(options.sink, instance, "opened")
  })

/** The cells of `cells` that `instance` collects. */
const cellsOf = (instance: Instance, cells: ReadonlyArray<Cell.Cell>): ReadonlyArray<Cell.Cell> =>
  instance.lanes === undefined ? cells : cells.filter((cell) => instance.lanes!.includes(cell.lane))

const NO_ANSWERS: Answers = { verifications: [], attachments: [] }

/** `not checkable`, saying `message`, for each verification `instance` names. */
const notCheckableLines = (instance: Instance, message: string): Answers => ({
  verifications: Collector.answeredBy(instance.collector.manifest).map((name) => ({
    name,
    source: instance.collector.name,
    answer: "not-checkable" as const,
    message
  })),
  attachments: []
})

/** What `instance` answers where it could not collect over `cells`: `not checkable`, saying `message`, for each verification it names. */
export const notCheckable = (instance: Instance, cells: ReadonlyArray<Cell.Cell>, message: string): Collected => {
  const lines = notCheckableLines(instance, message)
  return { label: instance.label, scope: lines, cells: new Map(cells.map((cell) => [cell.id, lines])), keyed: [], assigned: [], keys: [] }
}

/** `collected` with each cell of `unread` on `instance`'s lanes answered `not checkable`, saying why it was not described. */
const withUnread = (instance: Instance, collected: Collected, unread: ReadonlyArray<Unread>): Collected => {
  const mine = instance.lanes === undefined ? unread : unread.filter((cell) => instance.lanes!.includes(cell.lane))
  if (mine.length === 0) return collected
  const cells = new Map(collected.cells)
  for (const cell of mine) cells.set(cell.id, notCheckableLines(instance, `the cell ${cell.id} was not described: ${cell.why}`))
  return { ...collected, cells }
}

/** Why `line` is no answer of `instance` over the cells `ids`, or `undefined`. */
const faultOf = (instance: Instance, ids: ReadonlySet<string>, line: Lines.Line): string | undefined => {
  const cell = "cell" in line.line ? line.line.cell : undefined
  if (cell !== undefined && !ids.has(cell)) return `names the cell ${cell}, which is not one of its scope`
  const manifest = instance.collector.manifest
  switch (line._tag) {
    case "Verification":
      return Collector.answeredBy(manifest).includes(line.line.verification)
        ? undefined
        : `answers ${line.line.verification}, which its manifest does not name`
    case "KeyedRecord":
      return manifest.join === undefined ? "a record by key, where its manifest states no [join] key" : undefined
    case "Attachment":
      return Attached.faultOf(writerOf(instance), line.line)
    default:
      return undefined
  }
}

/** The lines `instance`'s collect answered: its stdout's, then its {@link Lines.OUT_FILE}'s; or the first that is none. */
const linesOf = (instance: Instance, stdout: string, ids: ReadonlySet<string>): Result.Result<ReadonlyArray<Lines.Line>, string> => {
  const outFile = path.join(layoutOf(instance).out, Lines.OUT_FILE)
  const read: Array<Lines.Line> = []
  for (const [file, origin] of [[stdout, "its stdout"], [outFile, `SIPRIG_OUT/${Lines.OUT_FILE}`]] as const) {
    if (!fs.existsSync(file)) continue
    const parsed = Lines.parse(fs.readFileSync(file, "utf8"), origin)
    if (Result.isFailure(parsed)) return Result.fail(parsed.failure)
    for (const { at, line } of parsed.success) {
      const fault = faultOf(instance, ids, line)
      if (fault !== undefined) return Result.fail(`${at}: ${fault}`)
      read.push(line)
    }
  }
  return Result.succeed(read)
}

/** `instance` as the writer of its attachment lines. */
const writerOf = (instance: Instance): Attached.Writer => ({ id: instance.id, source: instance.collector.name, out: layoutOf(instance).out })

/** One holder's answers as they accrue: its lines by name, the last kept, and its attachments. */
const accrued = () => ({ verifications: new Map<string, Verification.Verification>(), attachments: [] as Array<Attachment.Attachment> })

/** `lines`, read over `cells`, applied: each attachment's file copied and listed in its holder's directory. */
const applied = (instance: Instance, cells: ReadonlyArray<Cell.Cell>, lines: ReadonlyArray<Lines.Line>) =>
  Effect.try({
    try: (): Collected => {
      const dirs = new Map(cells.map((cell) => [cell.id, cell.dir]))
      const scope = accrued()
      const byCell = new Map(cells.map((cell) => [cell.id, accrued()]))
      const keyed: Array<Lines.KeyedRecord> = []
      const assigned: Array<Lines.AssignedRecord> = []
      const keys: Array<Lines.KeyLine> = []
      for (const line of lines) {
        switch (line._tag) {
          case "Verification": {
            const { verification, cell, ...rest } = line.line
            const holder = cell === undefined ? scope : byCell.get(cell)!
            holder.verifications.set(verification, { name: verification, source: instance.collector.name, ...rest })
            break
          }
          case "Attachment": {
            const cell = line.line.cell
            const holder = cell === undefined ? scope : byCell.get(cell)!
            holder.attachments.push(Attached.attach(writerOf(instance), cell === undefined ? instance.holder : dirs.get(cell)!, line.line))
            break
          }
          case "KeyedRecord":
            keyed.push(line.line)
            break
          case "AssignedRecord":
            assigned.push(line.line)
            break
          case "Key":
            keys.push(line.line)
            break
        }
      }
      const answersOf = (holder: ReturnType<typeof accrued>): Answers => ({
        verifications: [...holder.verifications.values()],
        attachments: holder.attachments
      })
      return {
        label: instance.label,
        scope: answersOf(scope),
        cells: new Map([...byCell].map(([id, holder]) => [id, answersOf(holder)])),
        keyed,
        assigned,
        keys
      }
    },
    catch: (cause) => `its answers not applied: ${errorText(cause)}`
  })

/**
 * `instance`'s `out/` removed once its answers were applied: every file it
 * attached is copied into its holder, so none is kept twice. A failed collect
 * keeps its `out/` to read what it left.
 */
const dropOut = (instance: Instance) => Effect.ignore(Effect.try(() => fs.rmSync(layoutOf(instance).out, { recursive: true, force: true })))

/** `opening`'s collect over `scopeCells`: its answers, or `not checkable` where it did not open or its collect failed. */
const collectOne = (opening: Opening, sink: Sink, scopeCells: ReadonlyArray<Cell.Cell>) =>
  Effect.gen(function*() {
    const { instance } = opening
    const cells = cellsOf(instance, scopeCells)
    if (opening.failure !== undefined) return notCheckable(instance, cells, opening.failure)
    const manifest = instance.collector.manifest
    const failed = (message: string) => Effect.as(emit(sink, instance, "failed", message), notCheckable(instance, cells, message))
    yield* emit(sink, instance, "collect")
    const end = yield* now
    const stdout = path.join(instance.dir, "collect.out.jsonl")
    const laid = yield* Effect.result(Effect.try({
      try: () => {
        fs.rmSync(path.join(layoutOf(instance).out, Lines.OUT_FILE), { force: true })
        writeContext(instance, { window: { start: opening.start, end: { lane_ms: end.clock_ms, wall_ms: end.wall_ms } }, cells })
      },
      catch: (cause) => `collect not run: its context not written: ${errorText(cause)}`
    }))
    if (Result.isFailure(laid)) return yield* failed(laid.failure)
    const ran = yield* runStep(instance, manifest.run.collect, "collect", Collector.boundsOf(manifest).collect_s * 1000, stdout)
    if (!Executable.succeeded(ran.ended)) return yield* failed(ran.said)
    const read = linesOf(instance, stdout, new Set(cells.map((cell) => cell.id)))
    if (Result.isFailure(read)) return yield* failed(`collect: ${read.failure}`)
    const done = yield* Effect.result(applied(instance, cells, read.success))
    if (Result.isFailure(done)) return yield* failed(`collect ${done.failure}`)
    yield* dropOut(instance)
    yield* emit(sink, instance, "collected")
    return done.success
  })

/** {@link collectOne}, a defect of the base's own reading (a file going between its check and its read) answered `not checkable` saying it died. */
const collectGuarded = (opening: Opening, sink: Sink, scopeCells: ReadonlyArray<Cell.Cell>) =>
  Effect.catchDefect(collectOne(opening, sink, scopeCells), (defect) => {
    const message = `collect died: ${errorText(defect)}`
    return Effect.as(emit(sink, opening.instance, "failed", message), notCheckable(opening.instance, cellsOf(opening.instance, scopeCells), message))
  })

/**
 * The cells of `cells` with a failed line by then: one of their own
 * (`cell.v1` `verifications`), or one an earlier instance of the scope
 * answered of them in `earlier`; an informative or waived failure fails none.
 */
export const failedOf = (cells: ReadonlyArray<Cell.Cell>, earlier: ReadonlyArray<Collected>): ReadonlyArray<Cell.Cell> =>
  cells.filter((cell) =>
    Verification.failing([...cell.verifications, ...earlier.flatMap((one) => one.cells.get(cell.id)?.verifications ?? [])]).length > 0
  )

/** What an `on-failure` instance answers where no cell of its scope failed: nothing. */
const unasked = (instance: Instance): Collected => ({
  label: instance.label,
  scope: NO_ANSWERS,
  cells: new Map(),
  keyed: [],
  assigned: [],
  keys: []
})

/**
 * `opening`'s answers over `cells`, after `earlier`, the answers of the
 * instances collected before it, and the cells it read: an `on-failure` one
 * collects over the failed cells alone and answers nothing where none failed;
 * any other collects over every cell of its lanes, each of `unread` answered
 * `not checkable`.
 */
const answersOf = (
  opening: Opening,
  sink: Sink,
  cells: ReadonlyArray<Cell.Cell>,
  unread: ReadonlyArray<Unread>,
  earlier: ReadonlyArray<Collected>
): Effect.Effect<ScopeJudging.Stepped> => {
  if (opening.instance.onFailure !== true) {
    return Effect.map(collectGuarded(opening, sink, cells), (one) => ({ collected: withUnread(opening.instance, one, unread) }))
  }
  const failed = failedOf(cellsOf(opening.instance, cells), earlier)
  const read = failed.map((cell) => cell.id)
  return failed.length === 0
    ? Effect.succeed({ collected: unasked(opening.instance), read })
    : Effect.map(collectGuarded(opening, sink, failed), (collected) => ({ collected, read }))
}

/** One `close` of `instance`; why it failed, or `undefined`. */
const closeAttempt = (instance: Instance, file: string, sink: Sink) =>
  Effect.gen(function*() {
    yield* emit(sink, instance, "close")
    const laid = yield* Effect.result(Effect.try({
      try: () => {
        const layout = layoutOf(instance)
        fs.mkdirSync(layout.state, { recursive: true })
        fs.mkdirSync(layout.out, { recursive: true })
      },
      catch: (cause) => `close not run: its directory ${instance.dir}: ${errorText(cause)}`
    }))
    if (Result.isFailure(laid)) {
      yield* emit(sink, instance, "failed", laid.failure)
      return laid.failure
    }
    const ran = yield* runStep(instance, file, "close", Collector.boundsOf(instance.collector.manifest).close_s * 1000)
    if (!Executable.succeeded(ran.ended)) {
      yield* emit(sink, instance, "failed", ran.said)
      return ran.said
    }
    yield* emit(sink, instance, "closed")
    return undefined
  })

/** Why an attempt that ended `ended` failed, or `undefined`; one that died failed with its defect. */
const whyOf = (ended: Exit.Exit<string | undefined>): string | undefined =>
  Exit.isSuccess(ended) ? ended.value : `close attempt died: ${Cause.pretty(ended.cause).split("\n")[0]}`

/**
 * Closes `instance`, uninterruptibly: up to {@link CLOSE_ATTEMPTS} runs of its
 * `close`, the last failed one followed by `lab-left-changed`. Whether it
 * closed; one with no `close` has nothing to undo.
 */
export const close = (instance: Instance, sink: Sink): Effect.Effect<boolean> =>
  Effect.uninterruptible(Effect.gen(function*() {
    const file = instance.collector.manifest.run.close
    if (file === undefined) return true
    let why: string | undefined
    for (let tried = 1; tried <= CLOSE_ATTEMPTS; tried++) {
      why = whyOf(yield* Effect.exit(closeAttempt(instance, file, sink)))
      if (why === undefined) return true
    }
    yield* emit(sink, instance, LAB_LEFT_CHANGED, `${CLOSE_ATTEMPTS} close attempts failed, the last: ${why}`)
    return false
  }))

/** What a scope's collectors run with. */
export interface Options {
  readonly sink: Sink
  readonly locker: Locker
  /** The judges of the scope's exports alone (`scope-judging.ts`); none where absent. */
  readonly exportJudges?: ScopeJudging.ExportJudges | undefined
}

/**
 * Opens `instances` in order for the scope at hand, once the scope's locks
 * are taken: each its directory laid out empty and its context written, then
 * its `open` run within its bound. The scope's finalizer closes, in reverse
 * order and uninterruptibly, every instance that went past the locks, then
 * gives the locks back: after the scope collected, failed or was
 * interrupted, an instance whose `open` failed or was interrupted included.
 * An interruption interrupts the lock wait or the `open` under way, never a
 * `close`.
 */
export const opened = (instances: ReadonlyArray<Instance>, options: Options): Effect.Effect<Opened, never, Scope.Scope> =>
  Effect.uninterruptibleMask((restore) =>
    Effect.gen(function*() {
      const openings: Array<Opening> = []
      const left: Array<string> = []
      let release: Effect.Effect<void> = Effect.void
      yield* Effect.addFinalizer(() =>
        Effect.andThen(
          Effect.forEach([...openings].reverse(), (opening) =>
            opening.closes ? Effect.gen(function*() {
              const closed = yield* Effect.exit(close(opening.instance, options.sink))
              if (!(Exit.isSuccess(closed) && closed.value)) left.push(opening.instance.label)
            }) : Effect.void, { discard: true }),
          release
        )
      )
      const locks = scopeLocksOf(instances)
      const taken = locks.names.length === 0
        ? Result.succeed(Effect.void)
        : yield* restore(Effect.result(options.locker(locks.names, locks.holder)))
      if (Result.isSuccess(taken)) release = taken.success
      const refused = Result.isFailure(taken) ? taken.failure : undefined
      for (const instance of instances) {
        const at = yield* now
        const opening: Opening = { instance, start: { lane_ms: at.clock_ms, wall_ms: at.wall_ms }, closes: false, failure: undefined }
        openings.push(opening)
        yield* openOne(opening, refused, options, restore)
      }
      let collected = false
      const start = openings[0]?.start ?? (yield* Effect.map(now, (at) => ({ lane_ms: at.clock_ms, wall_ms: at.wall_ms })))
      return {
        collect: (cells, unread = []) =>
          Effect.suspend(() => {
            if (collected) return Effect.die(new Error("a scope's collectors collect once"))
            collected = true
            return ScopeJudging.collect({
              steps: openings.map((opening) => ({
                instance: opening.instance,
                collect: (current, earlier) => answersOf(opening, options.sink, current, unread, earlier)
              })),
              cells,
              undescribed: unread,
              exportJudges: options.exportJudges,
              start,
              say: (message) => Effect.flatMap(now, (at) => options.sink({ collector: "scope", phase: "failed", ...at, message }))
            })
          }),
        leftChanged: () => [...left]
      } satisfies Opened
    })
  )
