/**
 * `compare A B` — any two run sets, from any environment or lane, call by
 * call: the comparison of the calls both sides ran, printed as text or as the
 * `sip.compare/1` document (`@sip/corpus` `compare-report.ts`). Under the
 * store command's `compare` a bare integer names a SWEEP; everywhere else a
 * run.
 *
 * A side whose selector pins no lane (no `@lane:`, run, sweep or job) is read
 * on `--lane`, so the current population never mixes two lanes' cells of one
 * call; the document is `@sip/corpus` `compare-document.ts`'s, the one the
 * viewer serves. The exit code is 1 where a verdict flipped on the same code,
 * document and lane — the population nothing but the harness explains — so a
 * repeat run can gate on it — or where a call's repeats within a side
 * disagree (`unstable`); 2 on a selector the store cannot answer or that names
 * nothing.
 *
 * `--request [--repeat N]` freezes the calls that moved as a replay request
 * (`@sip/corpus` `compare-request.ts`) and prints how to replay it.
 * `--ladder-changed` lists the calls whose two replay ladders differ once each
 * run's minted ids are normalised, an unchanged verdict included; it never
 * changes the exit code.
 */
import { Identity } from "@sip/corpus"
import * as CompareDocument from "@sip/corpus/report/compare-document"
import * as Report from "@sip/corpus/report/compare-report"
import * as CompareRequest from "@sip/corpus/requests/compare-request"
import type { Compare } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { Argument, Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { pathFlags } from "../flags.js"
import { exit } from "../runner.js"

export const flags = (facts: CliFacts) => ({
  before: Argument.string("before").pipe(Argument.withDescription(`The before side: ${facts.selectorForms}`)),
  after: Argument.string("after").pipe(Argument.withDescription("The after side, the same forms")),
  lane: Flag.string("lane").pipe(
    Flag.withDescription("The lane a side that pins none is read on"),
    Flag.withDefault(facts.defaultLane)
  ),
  json: Flag.boolean("json").pipe(Flag.withDescription("Print the JSON document"), Flag.withDefault(false)),
  all: Flag.boolean("all").pipe(
    Flag.withDescription("List every compared call, the unchanged ones too"),
    Flag.withDefault(false)
  ),
  ladderChanged: Flag.boolean("ladder-changed").pipe(
    Flag.withDescription(
      "List only the calls whose replay ladders differ once each run's minted ids are normalised, whatever their " +
        "class (among the first --ladders compared calls)"
    ),
    Flag.withDefault(false)
  ),
  ladders: Flag.integer("ladders").pipe(
    Flag.withDescription("How many listed calls get their ladders read"),
    Flag.withDefault(50)
  ),
  limit: Flag.integer("limit").pipe(Flag.withDescription("Calls the text prints"), Flag.withDefault(20)),
  only: Flag.integer("only").pipe(
    Flag.withDescription("How many calls an only-before/only-after list names"),
    Flag.withDefault(100)
  ),
  request: Flag.boolean("request").pipe(
    Flag.withDescription("Freeze the calls that moved as a replay request"),
    Flag.withDefault(false)
  ),
  repeat: Flag.integer("repeat").pipe(
    Flag.withDescription("--request: passes each job of the request replays its cells in"),
    Flag.withDefault(1)
  )
})

interface Input {
  readonly before: string
  readonly after: string
  readonly lane: string
  readonly json: boolean
  readonly all: boolean
  readonly ladderChanged: boolean
  readonly ladders: number
  readonly limit: number
  readonly only: number
  readonly request: boolean
  readonly repeat: number
}

/** Who makes a request: the environment runs are recorded under, else the shell's user. */
export const createdBy = Effect.map(
  Identity.Service,
  (identity) => identity.environment ?? process.env.USER ?? "unknown"
)

/** The moved calls frozen as a request, and the lines saying what came of it. */
const requestLines = Effect.fn("Compare.requestLines")(function*(
  facts: CliFacts,
  comparison: Compare.Comparison,
  input: Input
) {
  const made = yield* CompareRequest.request(comparison, {
    lane: input.lane,
    repeat: input.repeat,
    createdBy: yield* createdBy
  })
  if (made._tag === "refused") return [`[compare] no request: ${made.why}`]
  return [
    `[compare] request ${made.id}: ${made.cells} moved call(s)` +
    (input.repeat > 1 ? `, ${input.repeat} passes per job` : ""),
    ...(made.unkeyed.length === 0 ? [] : [`  not keyed (no frozen id): ${made.unkeyed.join(", ")}`]),
    `  replay it: ${facts.invocation("replay")} --request ${made.id}   ` +
    `then: ${facts.invocation("compare")} ${comparison.before.selector} @job:<its job>`
  ]
})

/** The command's handler; `sweeps` reads a bare integer as a sweep (the store command's `compare`). */
export const handler = (facts: CliFacts, sweeps: boolean) => (input: Input) =>
  Effect.gen(function*() {
    const repeat = CompareRequest.repeatRefusal(input.request, input.repeat)
    if (repeat !== null) {
      yield* Console.error(`[compare] refused: ${repeat}`)
      return yield* exit(2)
    }
    const answer = yield* CompareDocument.compare({ ...input, sweeps, viewer: facts.viewer() })
    if (answer._tag === "refused") {
      yield* Console.error(`[compare] refused: ${answer.reason}`)
      return yield* exit(2)
    }
    const doc = answer.document
    yield* Console.log(input.json ? JSON.stringify(doc, null, 2) : Report.render(doc, Math.max(0, input.limit)))
    if (input.request) for (const line of yield* requestLines(facts, answer.comparison, input)) yield* Console.error(line)
    yield* exit(doc.unexplained > 0 ? 1 : 0)
  })

/** `compare A B`, its roots on its line. */
export const command = (facts: CliFacts) =>
  Command.make("compare", { ...pathFlags, ...flags(facts) }, handler(facts, false)).pipe(
    Command.withDescription(`What moved between two run sets, call by call\n\n${facts.frontDoors}`)
  )
