/**
 * `evidence` — triage evidence in one command: cut then replay, for a session
 * that wants both.
 *
 *   evidence <capture>.pcap.gz...       # targeted: a new run, the cut reused while current
 *   evidence --case ID <capture>        # one call: a cases run, the cut reused while current
 *   evidence --force-recut <capture>    # targeted: a fresh cut, whatever stands
 *   evidence --known [--strict]         # pending registry records + current failures
 *   evidence --all                      # the whole standing cut: the no-regression sweep
 *
 * `--known` replays the queue of what already fails, so a capture of it that
 * errors is the queue's own: it exits 0 with the closing summary, and 1 only
 * under `--strict` (the CI meaning), as the replay command does
 * (`errorsFail`). `--case` takes any cited form of a case id
 * (`case-selection.ts`).
 *
 * A named capture, whole or per call (`--case`), takes the campaign's
 * staleness rule (`@sip/corpus` `cut-staleness.ts`): a capture whose standing
 * cut came from this cut code and is no older than its artifacts is replayed
 * off that cut, and one that is stale or never cut is cut again, saying why;
 * `--force-recut` cuts a current one. A derived universe cuts.
 *
 * The two phases are separate commands for a reason — one cut feeds as many
 * lanes at once as the host has. This composes them for the common case: one
 * capture, one lane, one answer.
 *
 * The replay is pinned to the prep the cut just opened, so this command answers
 * for the documents it cut and for no others, whatever else lands beside it.
 * What the cut refused lands in `<stem>/<capture>/excluded.json` under the
 * session tree, and each replay in a `run<id>` holding its cells, their
 * run-specs and its own index; the id comes from the store, so no two replays
 * collide on a run directory.
 */
import { afterSweep } from "@sip/corpus/housekeeping/cleanup"
import { errorsFail } from "@sip/corpus/options"
import * as Population from "@sip/corpus/population"
import * as ReplayBuild from "@sip/corpus/replay/build"
import { prepOf, workAsOf } from "@sip/corpus/replay/evidence-prep"
import { planWork } from "@sip/corpus/replay/plan"
import { phase as replay, report } from "@sip/corpus/replay/replay"
import * as ReplaySummary from "@sip/corpus/report/replay-summary"
import { label, workList } from "@sip/corpus/universe"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import { Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { triageFlags, type TriageInput } from "../flags.js"
import { optionsOf, USAGE_TAIL } from "../options.js"
import * as Refusals from "../refusals.js"
import { exit } from "../runner.js"
import * as CaseSelection from "./case-selection.js"

export const flags = (facts: CliFacts) => ({
  ...triageFlags(facts),
  summary: Flag.boolean("summary").pipe(
    Flag.withDescription("Close with one line per cell: its first failing step beside the call's current run"),
    Flag.withDefault(false)
  )
})

type Input = TriageInput & { readonly summary: boolean }

const main = Effect.fn("Evidence.main")(function*(facts: CliFacts, input: Input) {
  const asked = yield* optionsOf(input)
  const away = yield* Refusals.environment(facts, asked.lane, asked.selection.universe, asked.dryRun)
  if (away !== null) {
    yield* Console.error(`[evidence] refused: ${away}`)
    return 2
  }
  const read = yield* CaseSelection.resolve(asked.selection, facts.refusalWhy)
  if (read._tag === "refused") {
    yield* Console.error(`[evidence] refused: ${read.why}`)
    return 2
  }
  const options = { ...asked, selection: read.selection }
  const refused = yield* Population.refusal(options.paths, options.selection)
  if (refused !== null) {
    yield* Console.error(`[evidence] refused: ${refused}`)
    return 2
  }
  const unstated = yield* Refusals.lane(options.lane)
  if (unstated !== null) {
    yield* Console.error(`[evidence] refused: ${unstated}`)
    return 2
  }
  const census = yield* Refusals.census(facts, options.paths)
  if (census !== null) {
    yield* Console.error(`[evidence] refused: ${census}`)
    return 2
  }
  const captures = yield* workList(options.paths, options.lane, options.selection, options.prep)
  if (captures.length === 0) {
    yield* Console.error(
      options.selection.universe === "named"
        ? `usage: ${facts.invocation("evidence")} ${USAGE_TAIL} [--summary]`
        : "[evidence] nothing to do"
    )
    return options.selection.universe === "named" ? 2 : 0
  }
  if (options.dryRun) {
    for (const work of captures) yield* Console.log(label(work))
    return 0
  }
  const what = `${captures.length} capture(s) on ${options.lane}`
  // The lane is taken around BOTH phases: the cut needs nothing, but a command
  // that cut and then queued for its lane would replay a cut the wait aged.
  const replayed = yield* Effect.scoped(Effect.gen(function*() {
    yield* ReplayBuild.session(options, `cutting and replaying ${what}`)
    const prep = yield* prepOf(captures.map((work) => work.capture), options)
    if (prep._tag === "refused") return prep.why
    const work = yield* workAsOf(captures, options, prep.prep)
    if (work.length === 0) return `the cut under prep${prep.prep} left no call under the span bound`
    const plans = yield* planWork(work, prep.prep, options.concurrency)
    return yield* replay({ captures: plans, options, prepId: prep.prep })
  })).pipe(
    Effect.catchTag("Session.Held", (held) => Effect.succeed(held.held)),
    Effect.catchTag("ReplayDb.UnknownCases", (unknown) => Effect.succeed(unknown.message))
  )
  if (typeof replayed === "string") {
    yield* Console.error(`[evidence] refused: ${replayed}`)
    return 2
  }
  const code = yield* report(replayed, { errorsFail: errorsFail(options.selection.universe, input.strict) })
  if (input.summary) {
    for (const line of yield* ReplaySummary.read(`@sweep:${replayed.sweep}`, options.lane)) yield* Console.log(line)
  }
  // After the lane is given back, never under it: a sweep must not be what the
  // next one queues behind while it tidies.
  yield* afterSweep(options, replayed.outcomes.length)
  return code
})

export const command = (facts: CliFacts) =>
  Command.make("evidence", flags(facts), (input) => main(facts, input).pipe(Effect.flatMap(exit))).pipe(
    Command.withDescription(`Cut then replay, for a session that wants both\n\n${facts.frontDoors}`)
  )
