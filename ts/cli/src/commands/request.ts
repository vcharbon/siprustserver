/**
 * `request` — replay requests, from the command line:
 *
 *   request create --lane L [--on-lane Q] [--scope S] [--q TEXT] [--rule R] [--expected E] [--refusal R]
 *   request create --lane L --capture C...
 *   request create --lane L --all | --standing [--max-span S] [--green-on L] [--sample N] [--limit N]
 *   request create --lane L --cells FILE
 *   … [--repeat N]   (each job replays the cells in N passes; `compare` reads split repeats as unstable)
 *     [--prep N] [--concurrency N] [--batch N] [--memory-max SIZE] [--starts-per-second N] [--no-cleanup]
 *     [--whole-first C...] [--dry-run]
 *   (an unstated --concurrency, --batch or --starts-per-second is the lane's default, else the generic one)
 *   request resolve <id> [--prep N]
 *   request list [--limit N]
 *   request show <id>
 *
 * `create` freezes what ONE selection form names — the filter bar's words
 * (read on `--on-lane`'s queue where given, else the request's own: one
 * lane's red calls requested on another), a capture list, the universe, or a
 * file of cells — and prints the id, the cell count and the captures;
 * `--dry-run` prints what it would come to — per capture full or cases — and
 * writes nothing; `--whole-first C…` widens the selection to those captures
 * whole. `resolve` prints what the cells come to against a prep and grows a
 * whole-capture request by what that prep holds; it runs nothing. The batch
 * that replays a request is `replay`'s.
 */
import { DEFAULT_CLEANUP, DEFAULT_MEMORY_MAX } from "@sip/corpus/knobs"
import * as KnobResolution from "@sip/corpus/replay/knob-resolution"
import { pin } from "@sip/corpus/replay/plan"
import { repeatRefusal } from "@sip/corpus/requests/compare-request"
import {
  createdLine,
  detailLines,
  freeze,
  preview,
  previewLines,
  Refused,
  requestLine,
  resolutionLines,
  type SelectionInput,
  widen
} from "@sip/corpus/requests/request"
import type { Selection as UniverseSelection } from "@sip/corpus/universe"
import { Facets, Requests } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { Argument, Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { laneFlag, pathFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { overridesOf, pathsOf, statedKnobs } from "../options.js"
import * as Refusals from "../refusals.js"
import { exit } from "../runner.js"
import { createdBy } from "./compare.js"

const prepFlag = IdFlags.flag("prep", "prep").pipe(
  Flag.withDescription("The prep the cut is read as of; absent pins the highest there is"),
  Flag.optional
)

export const createFlags = (facts: CliFacts) => ({
  lane: laneFlag(facts),
  onLane: Flag.string("on-lane").pipe(
    Flag.withDescription("The lane whose queue the filter bar's words are read on (default: --lane)"),
    Flag.optional
  ),
  scope: Flag.string("scope").pipe(
    Flag.withDescription("The filter bar's scope: failed | refused | red | all | passed | skipped"),
    Flag.optional
  ),
  q: Flag.string("q").pipe(Flag.withDescription("The filter bar's capture substring"), Flag.optional),
  rule: Flag.string("rule").pipe(Flag.withDescription("The filter bar's rule chip"), Flag.optional),
  expected: Flag.string("expected").pipe(Flag.withDescription("The filter bar's expected chip"), Flag.optional),
  refusal: Flag.string("refusal").pipe(Flag.withDescription("The filter bar's refusal chip"), Flag.optional),
  capture: Flag.string("capture").pipe(
    Flag.withDescription("A capture meant whole, extension and all (repeatable)"),
    Flag.atLeast(0)
  ),
  all: Flag.boolean("all").pipe(
    Flag.withDescription("The whole universe: the standing cut and every recorded capture"),
    Flag.withDefault(false)
  ),
  standing: Flag.boolean("standing").pipe(
    Flag.withDescription("The standing cut alone"),
    Flag.withDefault(false)
  ),
  maxSpan: Flag.float("max-span").pipe(
    Flag.withDescription("Only the calls spanning at most this many seconds"),
    Flag.optional
  ),
  greenOn: Flag.string("green-on").pipe(
    Flag.withDescription("Only captures whose latest run on this lane was green"),
    Flag.optional
  ),
  sample: Flag.integer("sample").pipe(
    Flag.withDescription("A stable sample of this many captures"),
    Flag.optional
  ),
  limit: Flag.integer("limit").pipe(
    Flag.withDescription("At most this many captures, in capture order"),
    Flag.optional
  ),
  cells: Flag.string("cells").pipe(
    Flag.withDescription("A file of cells, `<capture> <call key>` per line"),
    Flag.optional
  ),
  prep: prepFlag,
  concurrency: Flag.integer("concurrency").pipe(
    Flag.withDescription("Cells at once (default: the lane's)"),
    Flag.optional
  ),
  batch: Flag.integer("batch").pipe(
    Flag.withDescription("Cells per batch (default: the lane's)"),
    Flag.optional
  ),
  memoryMax: Flag.string("memory-max").pipe(
    Flag.withDescription("The memory scope the driver runs under"),
    Flag.withDefault(DEFAULT_MEMORY_MAX)
  ),
  startsPerSecond: Flag.float("starts-per-second").pipe(
    Flag.withDescription("Cell starts per second, 0 as fast as slots free (default: the lane's)"),
    Flag.optional
  ),
  cleanup: Flag.boolean("cleanup").pipe(
    Flag.withDescription("Run the retention pass after the replay; --no-cleanup keeps the tree as it left it"),
    Flag.withDefault(DEFAULT_CLEANUP)
  ),
  repeat: Flag.integer("repeat").pipe(
    Flag.withDescription("Passes each job replays the cells in; repeats that disagree compare as unstable"),
    Flag.withDefault(1)
  ),
  wholeFirst: Flag.string("whole-first").pipe(
    Flag.withDescription("A capture the selection is widened to whole (repeatable)"),
    Flag.atLeast(0)
  ),
  dryRun: Flag.boolean("dry-run").pipe(
    Flag.withDescription("Print what the selection comes to on the lane and create nothing"),
    Flag.withDefault(false)
  )
})

type CreateInput = Command.Command.Config.Infer<ReturnType<typeof createFlags>>

/** The knobs the request is created with: each as the line states it, else the lane's default, else the generic one. */
export const createKnobs = Effect.fn("RequestCli.createKnobs")(function*(input: CreateInput) {
  const knobs = KnobResolution.resolve(statedKnobs(input), undefined, yield* KnobResolution.forLane(input.lane))
  return {
    concurrency: Math.max(1, knobs.concurrency),
    batchSize: Math.max(1, knobs.batchSize),
    memoryMax: input.memoryMax,
    startsPerSecond: Math.max(0, knobs.startsPerSecond),
    cleanup: input.cleanup,
    ...(input.repeat > 1 ? { repeat: input.repeat } : {})
  } satisfies Requests.Knobs
})

/** The one selection form the line states, or why it states none or two. */
const selectionOf = (input: CreateInput): SelectionInput | Refused => {
  const filter = [input.scope, input.q, input.rule, input.expected, input.refusal, input.onLane].some(Option.isSome)
  const forms = [
    input.capture.length > 0 ? "--capture" : undefined,
    input.all || input.standing ? (input.standing ? "--standing" : "--all") : undefined,
    Option.isSome(input.cells) ? "--cells" : undefined,
    filter ? "the filter bar" : undefined
  ].filter((form): form is string => form !== undefined)
  if (forms.length > 1) return new Refused(`one selection form per request; got ${forms.join(" and ")}`)
  const predicates = [
    Option.isSome(input.maxSpan),
    Option.isSome(input.greenOn),
    Option.isSome(input.sample),
    Option.isSome(input.limit)
  ].some(Boolean)
  if (predicates && !(input.all || input.standing)) {
    return new Refused("--max-span, --green-on, --sample and --limit narrow --all or --standing")
  }
  if (input.capture.length > 0) return { kind: "captures", captures: input.capture }
  if (Option.isSome(input.cells)) return { kind: "cells", file: input.cells.value }
  if (input.all || input.standing) {
    const selection: UniverseSelection = {
      universe: input.standing ? "standing" : "all",
      positional: [],
      onlyNew: false,
      maxSpanSeconds: Option.getOrUndefined(input.maxSpan),
      greenOnLane: Option.getOrUndefined(input.greenOn),
      sampleSize: Option.getOrUndefined(input.sample),
      limit: Option.getOrUndefined(input.limit),
      cases: []
    }
    return { kind: "universe", selection }
  }
  const scope = Option.getOrUndefined(input.scope)
  if (scope !== undefined && !(Facets.SCOPES as ReadonlyArray<string>).includes(scope)) {
    return new Refused(`--scope ${scope} is none of ${Facets.SCOPES.join(", ")}`)
  }
  return {
    kind: "filter",
    lane: Option.getOrUndefined(input.onLane),
    query: {
      scope: scope as Facets.Scope | undefined,
      capture: Option.getOrUndefined(input.q),
      rule: Option.getOrUndefined(input.rule),
      expected: Option.getOrUndefined(input.expected),
      refusal: Option.getOrUndefined(input.refusal)
    }
  }
}

/** The root: the path flags every subcommand reads, and a usage line where none was named. */
const rootOf = (facts: CliFacts) =>
  Command.make("request", {}, () =>
    Effect.gen(function*() {
      yield* Console.error(
        `usage: ${facts.invocation("request")} create --lane L [selection…] | resolve <id> [--prep N] | ` +
          "list [--limit N] | show <id>"
      )
      yield* exit(2)
    })).pipe(
      Command.withDescription("Replay requests in the store"),
      Command.withSharedFlags(pathFlags)
    )

const createOf = (facts: CliFacts, root: ReturnType<typeof rootOf>) =>
  Command.make("create", createFlags(facts), (input) =>
    Effect.gen(function*() {
      const unstated = yield* Refusals.lane(input.lane)
      if (unstated !== null) {
        yield* Console.error(`[request] refused: ${unstated}`)
        return yield* exit(2)
      }
      const selection = selectionOf(input)
      if (selection instanceof Refused) {
        yield* Console.error(`[request] refused: ${selection.message}`)
        return yield* exit(2)
      }
      const repeat = repeatRefusal(true, input.repeat)
      if (repeat !== null) {
        yield* Console.error(`[request] refused: ${repeat}`)
        return yield* exit(2)
      }
      const prep = yield* pin(Option.getOrUndefined(input.prep))
      if (prep === null) {
        yield* Console.error(
          Option.isNone(input.prep)
            ? `[request] refused: the cut store holds no cut — run \`${facts.invocation("cut")}\` first`
            : `[request] refused: there is no prep ${input.prep.value}`
        )
        return yield* exit(2)
      }
      const paths = yield* Effect.flatMap(root, (shared) => pathsOf(overridesOf(shared)))
      const bare = yield* freeze(paths, input.lane, prep, selection)
      const frozen = bare instanceof Refused ? bare : yield* widen(bare, prep, input.wholeFirst)
      if (frozen instanceof Refused) {
        yield* Console.error(`[request] refused: ${frozen.message}`)
        return yield* exit(2)
      }
      if (input.dryRun) {
        for (const line of previewLines(yield* preview(prep, frozen), input.lane)) yield* Console.log(line)
        return
      }
      if (frozen.cells.length === 0 && frozen.selection.whole.length === 0) {
        yield* Console.error("[request] refused: the selection names no call")
        return yield* exit(2)
      }
      const requests = yield* Requests.Service
      const created = yield* requests.create({
        lane: input.lane,
        selection: frozen.selection,
        knobs: yield* createKnobs(input),
        createdBy: yield* createdBy,
        cells: frozen.cells
      })
      yield* Console.log(createdLine(created, input.lane))
      for (const row of frozen.unkeyed) {
        yield* Console.error(`[request] ${row.capture} ${row.caseId}: no cut names a key for it — not on the request`)
      }
    })
  ).pipe(Command.withDescription("Freeze a selection as a request"))

const resolve = Command.make(
  "resolve",
  { id: IdFlags.argument("id", "request").pipe(Argument.withDescription("The request")), prep: prepFlag },
  (input) =>
    Effect.gen(function*() {
      const prep = yield* pin(Option.getOrUndefined(input.prep))
      if (prep === null) {
        yield* Console.error(
          Option.isNone(input.prep)
            ? "[request] refused: the cut store holds no cut"
            : `[request] refused: there is no prep ${input.prep.value}`
        )
        return yield* exit(2)
      }
      const requests = yield* Requests.Service
      const resolution = yield* requests.resolve(input.id, prep)
      if (resolution === null) {
        yield* Console.error(`[request] refused: no request ${input.id}`)
        return yield* exit(2)
      }
      for (const line of resolutionLines(resolution)) yield* Console.log(line)
    })
).pipe(Command.withDescription("What a request's cells come to against a prep"))

const list = Command.make(
  "list",
  { limit: Flag.integer("limit").pipe(Flag.withDescription("Rows to show"), Flag.withDefault(20)) },
  (input) =>
    Effect.gen(function*() {
      const requests = yield* Requests.Service
      for (const row of yield* requests.list(input.limit)) yield* Console.log(requestLine(row))
    })
).pipe(Command.withDescription("The requests, newest first"))

const show = Command.make(
  "show",
  { id: IdFlags.argument("id", "request").pipe(Argument.withDescription("The request")) },
  (input) =>
    Effect.gen(function*() {
      const requests = yield* Requests.Service
      const detail = yield* requests.show(input.id)
      if (detail === null) {
        yield* Console.error(`[request] refused: no request ${input.id}`)
        return yield* exit(2)
      }
      for (const line of detailLines(detail)) yield* Console.log(line)
    })
).pipe(Command.withDescription("A request, its cells and its jobs"))

export const command = (facts: CliFacts) => {
  const root = rootOf(facts)
  return root.pipe(Command.withSubcommands([createOf(facts, root), resolve, list, show]))
}
