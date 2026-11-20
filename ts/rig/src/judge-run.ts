/**
 * One judge's run over one scope (ADR-0060 appendix §6): called once with
 * every cell of the scope in `cell.v1`, its claims and the records of the
 * judged name joined to it, and the files of the unclaimed and the ambiguous
 * ones (`context.v1`'s `judging`). It answers `verification` and `attachment`
 * lines.
 *
 * A module judge's default export is called with the context and its
 * `SIPRIG_OUT`, and returns its lines as objects; an executable judge reads
 * `SIPRIG_CONTEXT` and answers on stdout and in `SIPRIG_OUT/lines.jsonl`, as a
 * collector does, `SIPRIG_STEP` being `judge`. Both forms' lines are read
 * alike: on the same input they answer the same. A judge that fails (a module
 * that throws, rejects, dies or returns no iterable of lines included), times
 * out, or answers a line that is none of its (another kind, a cell outside the
 * scope, a verification it may not answer, a file it does not hold) answers
 * `not checkable`, quoting why, for every verification it may answer, on every
 * cell and on the scope. Only an executable judge's timeout is enforced
 * whatever it does: a module judge is trusted code in the base's process,
 * timed out only while it awaits; one that blocks synchronously blocks the
 * base (ADR-0060 item 18).
 */
import type { Attachment, Verification } from "@sip/contracts"
import * as Cause from "effect/Cause"
import * as Duration from "effect/Duration"
import * as Effect from "effect/Effect"
import * as Result from "effect/Result"
import * as fs from "node:fs"
import * as path from "node:path"
import { pathToFileURL } from "node:url"
import * as Attached from "./attached.js"
import type * as Cell from "./cell.js"
import * as Lines from "./collector-lines.js"
import type * as CollectorRun from "./collector-run.js"
import * as Context from "./context.js"
import * as Executable from "./executable.js"
import type * as Judge from "./judge.js"
import type * as RigPath from "./rig-path.js"

/** The step an executable judge runs as. */
export const STEP = "judge"

/** A judge's timeout where its manifest states none, in seconds. */
export const DEFAULT_JUDGE_S = 120

/** What a module judge's default export is called with. */
export interface Input {
  readonly context: Context.Context
  /** The directory its attached files go into. */
  readonly out: string
}

/** A module judge's default export: its lines, as the objects an executable prints one per line. */
export type Judging = (
  input: Input
) => Iterable<object> | Promise<Iterable<object>> | Effect.Effect<Iterable<object>, unknown>

/** One run of a judge. */
export interface Request {
  readonly judge: RigPath.Defined<Judge.Judge>
  /** The context it reads: the scope's, with `judging` stated. */
  readonly context: Context.Context
  /** The verifications it may answer. */
  readonly answers: ReadonlyArray<string>
  /** Its own directory: its context file, `state/`, `out/`, its log and its stdout. */
  readonly dir: string
  /** The scope's own directory, which holds the attachments that name no cell. */
  readonly holder: string
}

const errorText = (cause: unknown): string => (cause instanceof Error ? cause.message.split("\n")[0]! : String(cause))

/** `judge`'s timeout in milliseconds. */
const timeoutMsOf = (judge: Judge.Judge): number => (judge.timeouts?.judge_s ?? DEFAULT_JUDGE_S) * 1000

const layoutOf = (dir: string) => ({
  context: path.join(dir, "context.json"),
  out: path.join(dir, "out"),
  state: path.join(dir, "state"),
  log: path.join(dir, `${STEP}.log`),
  stdout: path.join(dir, `${STEP}.out.jsonl`)
})

/** Some text of a judge's lines, and where it was read. */
interface Answered {
  readonly text: string
  readonly origin: string
}

/** The text of `answered`, what a module's lines came to, one JSON object each; or why it is none. */
const textOfLines = (answered: unknown): Result.Result<string, string> => {
  if (typeof answered !== "object" || answered === null || !(Symbol.iterator in answered)) {
    return Result.fail(`returned ${String(answered)}, no iterable of lines`)
  }
  try {
    return Result.succeed([...(answered as Iterable<unknown>)].map((line) => `${JSON.stringify(line)}\n`).join(""))
  } catch (cause) {
    return Result.fail(`its lines are not JSON: ${errorText(cause)}`)
  }
}

/** `answered`, whatever a module's default export returned, awaited: its lines' text, or why it is none. */
const settled = (file: string, answered: ReturnType<Judging>): Effect.Effect<string, string> =>
  Effect.flatMap(
    Effect.isEffect(answered)
      ? Effect.mapError(answered, (cause) => `module ${file} failed: ${errorText(cause)}`)
      : Effect.tryPromise({ try: async () => await answered, catch: (cause) => `module ${file} rejected: ${errorText(cause)}` }),
    (lines) => Effect.fromResult(Result.mapError(textOfLines(lines), (why) => `module ${file} ${why}`))
  )

/** Why a module judge's run, ended by `cause`, gave no lines: its failure, else what it died of, else its interruption. */
const whyOf = (file: string, cause: Cause.Cause<string>): string => {
  const failed = Cause.findError(cause)
  if (Result.isSuccess(failed)) return failed.success
  const died = Cause.findDefect(cause)
  return Result.isSuccess(died) ? `module ${file} died: ${errorText(died.success)}` : `module ${file} was interrupted`
}

/** Runs the module judge of `request`: its lines' text, or why it gave none, whatever ended it. */
const runModule = (request: Request, file: string): Effect.Effect<ReadonlyArray<Answered>, string> => {
  const layout = layoutOf(request.dir)
  const judged = Effect.gen(function*() {
    const loaded = yield* Effect.tryPromise({
      try: () => import(pathToFileURL(path.resolve(request.judge.dir, file)).href) as Promise<{ readonly default?: unknown }>,
      catch: (cause) => `module ${file} not loaded: ${errorText(cause)}`
    })
    if (typeof loaded.default !== "function") return yield* Effect.fail(`module ${file} exports no default function`)
    const judging = loaded.default as Judging
    const answered = yield* Effect.try({
      try: () => judging({ context: request.context, out: layout.out }),
      catch: (cause) => `module ${file} threw: ${errorText(cause)}`
    })
    return yield* settled(file, answered)
  })
  const ms = timeoutMsOf(request.judge.manifest)
  return judged.pipe(
    Effect.timeoutOrElse({ duration: Duration.millis(ms), orElse: () => Effect.fail(`module ${file} timed out after ${ms} ms`) }),
    Effect.tap((text) => Effect.try({ try: () => fs.writeFileSync(layout.stdout, text), catch: (cause) => `its lines not kept: ${errorText(cause)}` })),
    Effect.catchCause((cause) => Effect.fail(whyOf(file, cause))),
    Effect.map((text) => [{ text, origin: "its lines" }])
  )
}

/** Runs the executable judge of `request`: its stdout's text, then its `SIPRIG_OUT/lines.jsonl`'s, or why it gave none, its log's path said. */
const runExecutable = (request: Request, file: string): Effect.Effect<ReadonlyArray<Answered>, string> => {
  const layout = layoutOf(request.dir)
  const outFile = path.join(layout.out, Lines.OUT_FILE)
  return Effect.flatMap(
    Executable.run({ dir: request.judge.dir, file, ...layout, step: STEP, timeoutMs: timeoutMsOf(request.judge.manifest) }),
    (ended) =>
      Executable.succeeded(ended)
        ? Effect.try({
          try: () => [
            { text: fs.existsSync(layout.stdout) ? fs.readFileSync(layout.stdout, "utf8") : "", origin: "its stdout" },
            { text: fs.existsSync(outFile) ? fs.readFileSync(outFile, "utf8") : "", origin: `SIPRIG_OUT/${Lines.OUT_FILE}` }
          ],
          catch: (cause) => `its lines not read: ${errorText(cause)}`
        })
        : Effect.fail(`${STEP} ${Executable.describe(ended)}, its log ${layout.log}`)
  )
}

/** Why `line` is no answer of `request`'s judge, or `undefined`. */
const faultOf = (request: Request, ids: ReadonlySet<string>, writer: Attached.Writer, line: Lines.Line): string | undefined => {
  const cell = "cell" in line.line ? line.line.cell : undefined
  if (cell !== undefined && !ids.has(cell)) return `names the cell ${cell}, which is not one of its scope`
  switch (line._tag) {
    case "Verification":
      return request.answers.includes(line.line.verification) ? undefined : `answers ${line.line.verification}, which it may not answer`
    case "Attachment":
      return Attached.faultOf(writer, line.line)
    default:
      return "a judge answers verification and attachment lines only"
  }
}

/** The lines `answered` holds, each one of `request`'s judge; or the first that is none, and why. */
const linesOf = (request: Request, answered: ReadonlyArray<Answered>, writer: Attached.Writer): Result.Result<ReadonlyArray<Lines.Line>, string> => {
  const ids = new Set(request.context.cells.map((cell) => cell.id))
  const read: Array<Lines.Line> = []
  for (const { text, origin } of answered) {
    const parsed = Lines.parse(text, origin)
    if (Result.isFailure(parsed)) return Result.fail(parsed.failure)
    for (const { at, line } of parsed.success) {
      const fault = faultOf(request, ids, writer, line)
      if (fault !== undefined) return Result.fail(`${at}: ${fault}`)
      read.push(line)
    }
  }
  return Result.succeed(read)
}

/** The label a judge's answers go by: `judge <judge> of <name>`, `name` the records it judged. */
export const labelOf = (judge: string, name: string): string => `judge ${judge} of ${name}`

/** `not checkable`, saying `message`, for each verification `request` may answer, on every cell and on the scope. */
export const notCheckable = (request: Pick<Request, "judge" | "answers" | "context">, message: string): CollectorRun.Collected => {
  const lines: CollectorRun.Answers = {
    verifications: request.answers.map((name) => ({ name, source: request.judge.name, answer: "not-checkable" as const, message })),
    attachments: []
  }
  return {
    label: labelOf(request.judge.name, request.context.judging?.name ?? request.judge.name),
    scope: lines,
    cells: new Map(request.context.cells.map((cell) => [cell.id, lines])),
    keyed: [],
    assigned: [],
    keys: []
  }
}

/** `lines` applied: each verification held by its cell or the scope, each attachment's file copied and listed in its holder's directory. */
const applied = (request: Request, writer: Attached.Writer, lines: ReadonlyArray<Lines.Line>): CollectorRun.Collected => {
  const cells: ReadonlyArray<Cell.Cell> = request.context.cells
  const holderOf = () => ({ verifications: new Map<string, Verification.Verification>(), attachments: [] as Array<Attachment.Attachment> })
  const scope = holderOf()
  const byCell = new Map(cells.map((cell) => [cell.id, holderOf()]))
  const dirs = new Map(cells.map((cell) => [cell.id, cell.dir]))
  for (const line of lines) {
    if (line._tag === "Verification") {
      const { verification, cell, ...rest } = line.line
      const held = cell === undefined ? scope : byCell.get(cell)!
      held.verifications.set(verification, { name: verification, source: request.judge.name, ...rest })
    } else if (line._tag === "Attachment") {
      const cell = line.line.cell
      const held = cell === undefined ? scope : byCell.get(cell)!
      held.attachments.push(Attached.attach(writer, cell === undefined ? request.holder : dirs.get(cell)!, line.line))
    }
  }
  const answersOf = (held: ReturnType<typeof holderOf>): CollectorRun.Answers => ({
    verifications: [...held.verifications.values()],
    attachments: held.attachments
  })
  return {
    label: labelOf(request.judge.name, request.context.judging?.name ?? request.judge.name),
    scope: answersOf(scope),
    cells: new Map([...byCell].map(([id, held]) => [id, answersOf(held)])),
    keyed: [],
    assigned: [],
    keys: []
  }
}

/** `request`'s judge run once, its lines applied: a defect of the base's own reading left to {@link run}. */
const answeredOf = (request: Request): Effect.Effect<CollectorRun.Collected> =>
  Effect.gen(function*() {
    const layout = layoutOf(request.dir)
    const writer: Attached.Writer = { id: `judge-${request.judge.name}`, source: request.judge.name, out: layout.out }
    const laid = yield* Effect.result(Effect.try({
      try: () => {
        fs.rmSync(request.dir, { recursive: true, force: true })
        fs.mkdirSync(layout.out, { recursive: true })
        fs.mkdirSync(layout.state, { recursive: true })
        fs.writeFileSync(layout.context, `${JSON.stringify(request.context, null, 2)}\n`)
      },
      catch: (cause) => `judge not run: its directory ${request.dir}: ${errorText(cause)}`
    }))
    if (Result.isFailure(laid)) return notCheckable(request, laid.failure)
    const { module, run: executable } = request.judge.manifest
    const answered = yield* Effect.result(
      module !== undefined ? runModule(request, module) : runExecutable(request, executable!)
    )
    if (Result.isFailure(answered)) return notCheckable(request, `judge ${request.judge.name}: ${answered.failure}`)
    const read = linesOf(request, answered.success, writer)
    if (Result.isFailure(read)) return notCheckable(request, `judge ${request.judge.name}: ${read.failure}`)
    const done = yield* Effect.result(Effect.try({ try: () => applied(request, writer, read.success), catch: errorText }))
    return Result.isFailure(done) ? notCheckable(request, `judge ${request.judge.name}: its answers not applied: ${done.failure}`) : done.success
  })

/** Runs `request`'s judge once: what it answered, or `not checkable` where it failed, whatever it failed of. */
export const run = (request: Request): Effect.Effect<CollectorRun.Collected> =>
  Effect.catchDefect(answeredOf(request), (defect) =>
    Effect.succeed(notCheckable(request, `judge ${request.judge.name} died: ${errorText(defect)}`)))
