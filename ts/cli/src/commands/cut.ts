/**
 * `cut` — the CUT, alone: raw corpus artifacts to the cut store.
 *
 *   cut <capture>.pcap.gz... | --known | --all | --standing [--stale-only]
 *   cut --all --slice 200 [--resume <prep>]
 *   cut --job <id> …            (what a launched cut job runs)
 *
 * A `<capture>` is the pcap FILE NAME, extension and all — it names a
 * directory under the corpus and a key in the run store. A bare stem is a
 * DIFFERENT capture that owns no flows document.
 *
 * It enriches, cuts and records, and takes nothing from anybody: a prep number
 * comes from a sequence and everything under it is content-named, so a cut runs
 * beside any replay and beside another cut. Run it once — then replay the prep
 * it printed, on as many lanes at once as the host has.
 *
 * `--stale-only` keeps of the list the captures whose standing cut is not
 * current — never cut, broken, taken by other cut code, or re-extracted since
 * (`@sip/corpus` `cut-staleness.ts`); without it every named capture is cut
 * again. `--slice` is for the corpus-wide re-cut: it reports per slice and the
 * prep it prints resumes the run with `--resume`. Every capture is reported as
 * it lands: cut, broke, or refused-all, with its prep.
 *
 * As a JOB (`--job`), the row is the one the job command made: the prep names
 * it, the list's size is its planned count, each slice its batch, and the row
 * closes with the exit code on the way out.
 *
 * It records no EVIDENCE: a cut is not evidence, the replay of it is. What it
 * does record is the cut itself — which prep cut which capture, and the content
 * its cases rebuild from.
 */
import { phase } from "@sip/corpus/cut/cut"
import { DEFAULT_SLICE, sliced } from "@sip/corpus/cut/cut-slices"
import { cutStaleness } from "@sip/corpus/cut/cut-staleness"
import type { Options } from "@sip/corpus/options"
import * as Population from "@sip/corpus/population"
import { workList } from "@sip/corpus/universe"
import { Cuts, Jobs } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { triageFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { optionsOf, USAGE_TAIL } from "../options.js"
import * as Refusals from "../refusals.js"
import { exit } from "../runner.js"

export const flags = (facts: CliFacts) => ({
  ...triageFlags(facts),
  staleOnly: Flag.boolean("stale-only").pipe(
    Flag.withDescription("Only the captures whose standing cut is not current (the campaign's rule)"),
    Flag.withDefault(false)
  ),
  job: IdFlags.flag("job", "job").pipe(
    Flag.withDescription(`The job row this process was launched for (${facts.invocation("job")} start cut)`),
    Flag.optional
  )
})

type Input = Command.Command.Config.Infer<ReturnType<typeof flags>>

/** The line a landed prep closes with: how to replay it. */
const ready = (facts: CliFacts, prep: number): string =>
  `[cut] prep${prep} ready — replay it with ` +
  `\`${facts.invocation("replay")} --lane <lane>\`, one invocation per lane, all at once`

/** Every named capture in one run, which is what a targeted cut wants. */
const whole = Effect.fn("Cut.whole")(function*(
  facts: CliFacts,
  captures: ReadonlyArray<string>,
  options: Options,
  job: number | undefined
) {
  const cut = yield* phase(captures, options, undefined, job)
  yield* Console.log(ready(facts, cut.prep))
  const broke = [...cut.readings.values()].some((reading) => reading.broke !== undefined)
  return broke || cut.mirrored.failed > 0 ? 1 : 0
})

/** The corpus in slices, reporting as each one lands. */
const slices = Effect.fn("Cut.slices")(function*(
  facts: CliFacts,
  captures: ReadonlyArray<string>,
  options: Options,
  size: number,
  resume: number | undefined,
  job: number | undefined
) {
  const cuts = yield* Cuts.Service
  const jobs = yield* Jobs.Service
  if (resume !== undefined && !(yield* cuts.exists(resume))) {
    yield* Console.error(`[cut] refused: there is no prep ${resume} to resume`)
    return 2
  }
  const done = yield* sliced({
    captures,
    options,
    size,
    ...(resume === undefined ? {} : { resume }),
    ...(job === undefined ? {} : { job, onSlice: (at: number, of: number) => jobs.batch(job, at, of).pipe(Effect.ignore) })
  })
  yield* Console.log(
    `[cut] prep${done.prep}: ${done.cut} cut, ${done.resumed} already cut, ${done.broke} broken — ` +
      `rows=${done.mirrored.recorded} rows-failed=${done.mirrored.failed} ` +
      `cases=${done.mirrored.cases} known=${done.mirrored.cases - done.mirrored.fresh} ` +
      `new=${done.mirrored.fresh}`
  )
  yield* Console.log(ready(facts, done.prep))
  return done.broke > 0 || done.mirrored.failed > 0 ? 1 : 0
})

/** The list under `--stale-only`: what `cut-staleness.ts` rules needs cutting, each reason printed. */
const staleOf = Effect.fn("Cut.stale")(function*(captures: ReadonlyArray<string>, options: Options) {
  const stale = yield* cutStaleness(captures, options.paths.corpus, new Set(), options.forceRecut)
  for (const one of stale) yield* Console.log(`[cut] stale  ${one.capture}  ${one.why}`)
  yield* Console.log(`[cut] stale-only: ${stale.length} of ${captures.length} capture(s) need cutting`)
  return stale.map((one) => one.capture)
})

const work = Effect.fn("Cut.work")(function*(facts: CliFacts, input: Input, options: Options, job: number | undefined) {
  const away = yield* Refusals.environment(facts, undefined, options.selection.universe, options.dryRun)
  if (away !== null) {
    yield* Console.error(`[cut] refused: ${away}`)
    return 2
  }
  const refused = yield* Population.refusal(options.paths, options.selection)
  if (refused !== null) {
    yield* Console.error(`[cut] refused: ${refused}`)
    return 2
  }
  const census = yield* Refusals.census(facts, options.paths)
  if (census !== null) {
    yield* Console.error(`[cut] refused: ${census}`)
    return 2
  }
  const named = (yield* workList(options.paths, options.lane, options.selection, options.prep))
    .map((work) => work.capture)
  if (named.length === 0) {
    const usage = `usage: ${facts.invocation("cut")} ${USAGE_TAIL} [--stale-only] [--slice N] [--resume <prep>] [--job <id>]`
    yield* Console.error(options.selection.universe === "named" ? usage : "[cut] nothing to do")
    return options.selection.universe === "named" ? 2 : 0
  }
  const captures = input.staleOnly ? yield* staleOf(named, options) : named
  if (options.dryRun) {
    for (const capture of captures) yield* Console.log(capture)
    return 0
  }
  if (job !== undefined) {
    const jobs = yield* Jobs.Service
    yield* jobs.planned(job, captures.length)
  }
  if (captures.length === 0) {
    yield* Console.log("[cut] every capture stands cut on this code")
    return 0
  }
  const slice = Option.getOrUndefined(input.slice)
  const resume = Option.getOrUndefined(input.resume)
  const cut = slice === undefined && resume === undefined
    ? whole(facts, captures, options, job)
    : slices(facts, captures, options, slice === undefined ? DEFAULT_SLICE : Math.max(1, slice), resume, job)
  return yield* cut.pipe(Effect.catchTag("Corpus.CensusRefused", (refused) =>
    Console.error(`[cut] refused: ${refused.reason}`).pipe(Effect.as(2))))
})

/** The exit code as the job's end: 0 is done, anything else failed. */
const closing = (code: number): Jobs.CloseRequest => ({ status: code === 0 ? "done" : "failed", exit: code })

/**
 * The work as a job where the line names one: the row must exist, and it
 * closes with the code whatever way the work ended — a defect that escaped
 * the phase closes it `failed` before it propagates.
 */
const main = Effect.fn("Cut.main")(function*(facts: CliFacts, input: Input) {
  const options = yield* optionsOf(input)
  const job = Option.getOrUndefined(input.job)
  if (job === undefined) return yield* work(facts, input, options, undefined)
  const jobs = yield* Jobs.Service
  if ((yield* jobs.show(job)) === null) {
    yield* Console.error(`[cut] refused: there is no job ${job}`)
    return 2
  }
  const code = yield* work(facts, input, options, job).pipe(
    Effect.tapCause(() => jobs.close(job, { status: "failed", exit: 1 }).pipe(Effect.ignore))
  )
  yield* jobs.close(job, closing(code))
  return code
})

export const command = (facts: CliFacts) =>
  Command.make("cut", flags(facts), (input) => main(facts, input).pipe(Effect.flatMap(exit))).pipe(
    Command.withDescription("The cut alone: raw corpus artifacts to the cut store")
  )
