/**
 * `defineCollector`: a collector written as one TypeScript module (ADR-0060
 * appendix §5). The module calls it with the steps it has; its manifest names
 * the module's executable for each of them, and the step that runs is the one
 * `SIPRIG_STEP` states, a step the module lacks doing nothing. Each step
 * reads the decoded `context.v1`, `SIPRIG_STATE` and `SIPRIG_OUT`, and
 * answers through {@link Io}: the helper writes the attached files and the
 * lines, so the module handles neither.
 *
 * A step that fails writes one line saying why on its stderr and exits 1,
 * which the runner reads as a failed step and quotes.
 */
import { NodeServices } from "@effect/platform-node"
import type { Attachment } from "@sip/contracts"
import * as Cause from "effect/Cause"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import * as fs from "node:fs"
import * as path from "node:path"
import * as Lines from "./collector-lines.js"
import * as Context from "./context.js"
import * as RunMain from "./run-main.js"

/** The steps of a collector. */
export const STEPS = ["open", "collect", "close"] as const
export type Step = (typeof STEPS)[number]

/** Why a step could not read what the runner gave it. */
export class Unreadable extends Schema.TaggedError<Unreadable>()("Rig.CollectorUnreadable", {
  reason: Schema.String
}) {
  override get message(): string {
    return this.reason
  }
}

/** An attachment as a step gives it: its file's content where the helper writes it, else a file it wrote under `out`. */
export interface Attached {
  /** The cell it is of; the scope's own where absent. */
  readonly cell?: string
  /** Unique within its holder; the collector instance's own name where absent. */
  readonly name?: string
  readonly kind: Attachment.Kind
  readonly title?: string
  /** Its path under `out`, or the URL of a `link`. */
  readonly file: string
  /** What the helper writes at `file` under `out`. */
  readonly content?: string | Uint8Array
}

/** What a step reads, and how it answers. */
export interface Io {
  readonly step: Step
  readonly context: Context.Context
  /** The directory kept from `open` to `close`. */
  readonly state: string
  /** The directory its attached files go into. */
  readonly out: string
  readonly answer: (line: Lines.VerificationLine) => Effect.Effect<void>
  readonly attach: (attached: Attached) => Effect.Effect<void, Unreadable>
  readonly record: (line: Lines.KeyedRecord | Lines.AssignedRecord) => Effect.Effect<void>
  readonly key: (line: Lines.KeyLine) => Effect.Effect<void>
}

/** A collector's steps; `collect` is the one every collector has. */
export interface Definition<E, R> {
  readonly open?: (io: Io) => Effect.Effect<void, E, R>
  readonly collect: (io: Io) => Effect.Effect<void, E, R>
  readonly close?: (io: Io) => Effect.Effect<void, E, R>
}

const decodeContext = Schema.decodeUnknownEffect(Context.Context)

/** The variable `name` of `env`, or why it is unset. */
const variable = (env: Readonly<Record<string, string | undefined>>, name: string) =>
  Effect.fromNullishOr(env[name]).pipe(Effect.mapError(() => new Unreadable({ reason: `${name} is not set` })))

const unreadable = (what: string) => (cause: unknown) =>
  new Unreadable({ reason: `${what}: ${cause instanceof Error ? cause.message.split("\n")[0] : String(cause)}` })

/** `file` as the path a line names: relative to `out`, or the URL of a `link`; why it is neither. */
const fileOf = (out: string, attached: Attached) =>
  attached.kind === "link" || (!path.isAbsolute(attached.file) && !attached.file.split(/[\\/]/).includes(".."))
    ? Effect.succeed(path.join(out, attached.file))
    : Effect.fail(new Unreadable({ reason: `attach: ${attached.file} is not a path under SIPRIG_OUT` }))

/**
 * The step `env` names of `definition`, run with what `env` gives it, its
 * lines written to `SIPRIG_OUT`'s `lines.jsonl` once it succeeded.
 */
export const program = <E, R>(
  definition: Definition<E, R>,
  env: Readonly<Record<string, string | undefined>> = process.env
): Effect.Effect<void, E | Unreadable, R> =>
  Effect.gen(function*() {
    const step = yield* variable(env, Context.VARIABLES.step)
    if (!(STEPS as ReadonlyArray<string>).includes(step)) {
      return yield* new Unreadable({ reason: `${Context.VARIABLES.step}: ${step} is no step of a collector` })
    }
    const run = definition[step as Step]
    if (run === undefined) return
    const contextFile = yield* variable(env, Context.VARIABLES.context)
    const out = yield* variable(env, Context.VARIABLES.out)
    const state = yield* variable(env, Context.VARIABLES.state)
    const raw = yield* Effect.try({ try: () => JSON.parse(fs.readFileSync(contextFile, "utf8")) as unknown, catch: unreadable(contextFile) })
    const context = yield* decodeContext(raw).pipe(Effect.mapError(unreadable(`${contextFile} is no ${Context.SCHEMA} file`)))
    const lines: Array<string> = []
    const push = (line: object) => Effect.sync(() => void lines.push(`${JSON.stringify(line)}\n`))
    const io: Io = {
      step: step as Step,
      context,
      state,
      out,
      answer: push,
      record: push,
      key: push,
      attach: (attached) =>
        Effect.gen(function*() {
          const target = yield* fileOf(out, attached)
          if (attached.content !== undefined && attached.kind !== "link") {
            yield* Effect.try({
              try: () => {
                fs.mkdirSync(path.dirname(target), { recursive: true })
                fs.writeFileSync(target, attached.content!)
              },
              catch: unreadable(`attach: ${attached.file} not written`)
            })
          }
          const { cell, content: _content, ...attachment } = attached
          yield* push({ attachment, ...(cell === undefined ? {} : { cell }) })
        })
    }
    yield* run(io)
    if (lines.length > 0) {
      yield* Effect.try({
        try: () => fs.appendFileSync(path.join(out, Lines.OUT_FILE), lines.join("")),
        catch: unreadable(`its lines not written to ${Lines.OUT_FILE}`)
      })
    }
  })

/** One line saying why `cause` failed the step. */
const sayOf = (cause: Cause.Cause<unknown>): string => {
  const error = Cause.squash(cause)
  const said = error instanceof Error ? error.message : String(error)
  return said.split("\n")[0] ?? "failed"
}

/**
 * Runs, as this process's main, the step of `definition` that `SIPRIG_STEP`
 * names, with the Node platform's services; exit status 0 once it did its
 * work, 1 with one line on stderr where it failed.
 */
export const defineCollector = <E>(definition: Definition<E, NodeServices.NodeServices>): void =>
  program(definition).pipe(
    Effect.tapCause((cause) => Effect.sync(() => void process.stderr.write(`${sayOf(cause)}\n`))),
    Effect.provide(NodeServices.layer),
    (main) => RunMain.runMain(main, { disableErrorReporting: true })
  )
