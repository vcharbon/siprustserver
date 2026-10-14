/**
 * The CAMPAIGN: pcaps on this host to runs on a lane, in one command.
 *
 * Six phases, each of which does only what is out of date — discover the pcaps,
 * extract the ones the population or the artifact store call stale
 * (`Source.staleness`: the judgement the source list prints), add them to the
 * population, cut the captures whose cut the code has moved under as a `cut`
 * job, check the lane, replay. Run it twice and the second run ingests nothing
 * and cuts nothing, because every phase compares a FINGERPRINT rather than a
 * timestamp or a directory listing.
 *
 * The replay is pinned to the prep this command's own cut opened, so it answers
 * for the documents it cut and for no others. The pin is a ceiling, which is
 * why the lane is taken after the cut rather than around it: a re-cut landing
 * while this command queues for its lane sits ABOVE the pin and changes nothing
 * it replays.
 */
import { Population } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as CutJob from "./cut/cut-job.js"
import { cutStaleness, type Stale } from "./cut/cut-staleness.js"
import { LaneHosting } from "./deployment/index.js"
import * as Fingerprint from "./fingerprint.js"
import { afterSweep } from "./housekeeping/cleanup.js"
import * as Discover from "./ingest/discover.js"
import * as Extract from "./ingest/extract.js"
import * as Source from "./ingest/source.js"
import type { Options } from "./options.js"
import * as ReplayBuild from "./replay/build.js"
import { pin, planWork } from "./replay/plan.js"
import { phase as replay, report } from "./replay/replay.js"
import { isPartial, type PartialWork, standing, type Work, workList } from "./universe.js"

/** What an operator asked this campaign to do. */
export interface Request {
  readonly options: Options
  /** The `--pcaps` arguments, as stated; relative ones resolve against `cwd`. */
  readonly pcaps: ReadonlyArray<string>
  /** The `--set` arguments: the files of ONE capture, as stated. */
  readonly set: ReadonlyArray<string>
  /** The OPERATOR's directory, which is never the pipeline's. */
  readonly cwd: string
  /** The lane to drive; absent is add-only. */
  readonly lane: string | undefined
  readonly forceRecut: boolean
  /** The deployment's own opening lines: what its host contributed. */
  readonly opening: ReadonlyArray<string>
  /** Where to watch a campaign on `lane`, as the deployment says it: printed before and after it. */
  readonly where: (lane: string) => ReadonlyArray<string>
}

/** What this invocation would do, which is what `--dry-run` prints. */
export interface Plan {
  readonly sources: ReadonlyArray<Discover.Source>
  readonly collisions: ReadonlyArray<string>
  readonly missing: ReadonlyArray<string>
  readonly needIngest: ReadonlyArray<Stale>
  readonly needCut: ReadonlyArray<Stale>
  readonly populationSize: number
  /** The work list, each capture with the case set its run names. */
  readonly replay: ReadonlyArray<Work>
}

/** The lines that say where to watch this campaign. */
const where = (request: Request, lane: string) =>
  Effect.forEach(request.where(lane), (line) => Console.log(`[campaign] ${line}`), { discard: true })

/** What this invocation would do, computed before it does any of it. */
export const plan = Effect.fn("Campaign.plan")(function*(request: Request) {
  const { options } = request
  const population = yield* Population.Service
  const found = Discover.join(
    yield* Discover.discover(request.pcaps, request.cwd),
    yield* Discover.set(request.set, request.cwd)
  )
  // Every capture under the generation it stands at.
  const sources = yield* Effect.forEach(found.sources, (source) => Extract.standing(options.paths.corpus, source))
  const staleness = yield* Source.staleness(sources, options.paths.corpus, yield* Fingerprint.extractor)
  const needIngest = [...staleness].map(([capture, why]) => ({ capture, why }))
  const captures = sources.map((source) => source.capture)
  const needCut = yield* cutStaleness(
    captures,
    options.paths.corpus,
    new Set(needIngest.map((one) => one.capture)),
    request.forceRecut
  )
  const replayList = request.lane === undefined ? [] : yield* workList(
    options.paths,
    request.lane,
    { ...options.selection, universe: "named", positional: captures },
    options.prep
  )
  return {
    sources,
    collisions: found.collisions,
    missing: found.missing,
    needIngest,
    needCut,
    populationSize: yield* population.size,
    replay: replayList
  } satisfies Plan
})

/** The captures a work list replays as a cases set: what a span bound kept in part. */
const partial = (work: ReadonlyArray<Work>): ReadonlyArray<PartialWork> => work.filter(isPartial)

/**
 * How the work list splits between full runs and cases sets, as the replay
 * lines print it, with the bound that split it; empty where nothing did.
 */
const split = (work: ReadonlyArray<Work>, options: Options): string => {
  const sets = partial(work).length
  if (sets === 0) return ""
  const bound = options.selection.maxSpanSeconds === undefined
    ? ""
    : ` (max-span ${options.selection.maxSpanSeconds} s)`
  return ` — ${work.length - sets} full, ${sets} as a cases set${bound}`
}

/** The plan as `--dry-run` prints it: one count per phase, in phase order. */
export const planLines = (plan: Plan, options: Options, lane: string | undefined): ReadonlyArray<string> => {
  const line = (what: string, count: number, tail = ""): string =>
    `[campaign] ${what.padEnd(16)} ${String(count).padStart(6)}${tail}`
  return [
    line("pcaps found", plan.sources.length),
    line("already ingested", plan.sources.length - plan.needIngest.length),
    line("needs ingest", plan.needIngest.length),
    line("already cut", plan.sources.length - plan.needCut.length),
    line("needs cut", plan.needCut.length),
    line("population", plan.populationSize),
    line(
      "replay",
      plan.replay.length,
      lane === undefined ? "  (add-only: no lane)" : `  after filters, on ${lane}${split(plan.replay, options)}`
    ),
    ...plan.needIngest.slice(0, 10).map((stale) => `   ingest  ${stale.capture}  ${stale.why}`),
    ...plan.needCut.slice(0, 10).map((stale) => `   cut     ${stale.capture}  ${stale.why}`),
    ...partial(plan.replay).slice(0, 10).map((work) =>
      `   cases   ${work.capture}  a ${work.set.cases.length}-case set: its calls under the span bound`
    )
  ]
}

/**
 * Every pcap extracted that needs it, and the population told what this host
 * now holds (`ingest/source.ts`): a capture whose extraction failed joins with
 * its failure, so the source list answers for it.
 */
const ingestPhase = Effect.fn("Campaign.ingest")(function*(request: Request, plan: Plan) {
  const { options } = request
  const population = yield* Population.Service
  // Before anything is added: a store with no population adopts the
  // deployment's own seed, once, so the first campaign on a host does not
  // silently shrink the population to the pcaps it was handed.
  yield* standing
  const ingested = yield* Source.ingest(plan.sources, options.paths.corpus, options.concurrency)
  let broke = 0
  let current = 0
  for (const [capture, outcome] of ingested.outcomes) {
    if (outcome._tag === "failed") {
      broke += 1
      yield* Console.error(Source.line(capture, outcome))
    } else if (outcome._tag === "current") current += 1
    else yield* Console.log(Source.line(capture, outcome))
  }
  yield* Console.log(
    `[campaign] ingest: ${ingested.extracted.size - broke} extracted, ${broke} refused, ${current} already current`
  )
  yield* Console.log(`[campaign] population: +${ingested.added} new, ${yield* population.size} capture(s) held`)
  // A capture that landed under a new generation is cut and replayed under it.
  const renamed = (stale: Stale): Stale => {
    const to = ingested.renamed.get(stale.capture)
    return to === undefined ? stale : { capture: to, why: `a new generation of ${stale.capture}` }
  }
  const landed: Plan = { ...plan, sources: ingested.sources, needCut: plan.needCut.map(renamed) }
  return { broke, landed }
})

/**
 * The captures that need one, cut as a `cut` JOB of this campaign's own
 * (`cut/cut-job.ts`: a row under this pid, the prep named after it) — and the
 * prep the replay then pins. Where nothing needed cutting, the pin is whatever
 * the store already stands at, so a re-run replays exactly what the first run
 * cut.
 */
const cutPhase = Effect.fn("Campaign.cut")(function*(request: Request, plan: Plan) {
  if (plan.needCut.length === 0) {
    yield* Console.log("[campaign] cut: every capture stands cut on this code")
    return yield* pin(request.options.prep)
  }
  const done = yield* CutJob.run(plan.needCut.map((one) => one.capture), request.options, "campaign")
  return done.prep
})

/** The lane driven over the pinned cut, under whatever locks the lane takes. */
const replayPhase = Effect.fn("Campaign.replay")(function*(
  request: Request,
  lane: string,
  captures: ReadonlyArray<Work>,
  prep: number
) {
  const refused = yield* Effect.flatMap(LaneHosting.Service, (hosting) => hosting.preflight(lane))
  if (refused !== null) {
    yield* Console.error(`[campaign] refused: ${lane} is not ready — ${refused}`)
    return 2
  }
  const options = { ...request.options, lane }
  const replayed = yield* Effect.scoped(Effect.gen(function*() {
    yield* ReplayBuild.session(options, `campaign over ${captures.length} capture(s) on ${lane}`)
    const plans = yield* planWork(captures, prep, options.concurrency)
    return yield* replay({ captures: plans, options, prepId: prep })
  })).pipe(
    Effect.catchTag("Session.Held", (held) => Effect.succeed(held.held)),
    Effect.catchTag("ReplayDb.UnknownCases", (unknown) => Effect.succeed(unknown.message))
  )
  if (typeof replayed === "string") {
    yield* Console.error(`[campaign] refused: ${replayed}`)
    return 2
  }
  yield* afterSweep(options, replayed.outcomes.length)
  return yield* report(replayed)
})

/** The whole campaign, as an exit code. */
export const run = Effect.fn("Campaign.run")(function*(request: Request) {
  const { options } = request
  const lane = request.lane
  for (const line of request.opening) yield* Console.log(`[campaign] ${line}`)
  yield* Console.log(`[campaign] store  ${options.paths.corpus}`)
  yield* Console.log(`[campaign] runs   ${options.paths.sessions}`)
  yield* where(request, lane ?? options.lane)

  const made = yield* plan(request)
  for (const said of [...made.missing, ...made.collisions]) yield* Console.error(`   ! ${said}`)
  if (made.missing.length > 0) {
    yield* Console.error("[campaign] refused: --pcaps or --set names a path this host does not hold")
    return 2
  }
  if (made.sources.length === 0) {
    yield* Console.error("[campaign] refused: --pcaps and --set named no pcap")
    return 2
  }
  for (const line of planLines(made, options, lane)) yield* Console.log(line)
  if (options.dryRun) return 0

  const { broke, landed } = yield* ingestPhase(request, made)
  const cut = yield* cutPhase(request, landed).pipe(
    Effect.map((prep) => ({ prep })),
    Effect.catchTag("Corpus.CensusRefused", (refused) => Effect.succeed({ refused: refused.reason }))
  )
  if ("refused" in cut) {
    yield* Console.error(`[campaign] refused: ${cut.refused}`)
    return 2
  }
  const prep = cut.prep
  if (lane === undefined) {
    yield* Console.log("[campaign] add-only: no lane was named, so nothing was replayed")
    yield* where(request, options.lane)
    return broke > 0 ? 1 : 0
  }
  if (prep === null) {
    yield* Console.error(
      options.prep === undefined
        ? "[campaign] refused: the cut store holds no cut to replay"
        : `[campaign] refused: there is no prep ${options.prep} to replay`
    )
    return 2
  }
  // Re-asked after the cut: the work list is a property of the CUT store, and
  // until this campaign's cut landed every capture it just extracted had no
  // span for `--max-span` to rule on.
  const captures = yield* workList(
    options.paths,
    lane,
    { ...options.selection, universe: "named", positional: landed.sources.map((one) => one.capture) },
    prep
  )
  if (captures.length === 0) {
    yield* Console.error("[campaign] nothing to replay: the filters left no capture")
    yield* where(request, lane)
    return broke > 0 ? 1 : 0
  }
  yield* Console.log(`[campaign] replay: ${captures.length} capture(s) on ${lane}, prep${prep}${split(captures, options)}`)
  const code = yield* replayPhase(request, lane, captures, prep)
  yield* where(request, lane)
  return code === 0 && broke > 0 ? 1 : code
})
