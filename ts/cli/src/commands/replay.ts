/**
 * `replay` — the REPLAY, alone: a standing cut driven against one lane.
 *
 *   replay [--prep N] --lane L <capture>.pcap.gz... | --known | --all
 *   replay --request <id> [--job <id>] [--after <job>] [--unran] [--prep N] [--batch N]
 *          [--concurrency N] [--memory-max SIZE] [--starts-per-second N] [--no-cleanup] [--dry-run]
 *   replay --from-census <census.json> --rule R [--prep N] [--dry-run]
 *   … [--summary]
 *
 * Every id takes every form it is cited in (`@sip/corpus` `id-forms.ts`):
 * `--request 15` or `request15`, `--prep prep110`; a `--case` is any cited
 * form of a case id, its capture named for it where the line names none
 * (`case-selection.ts`). `--from-census` replays, as cases sets, the cells a
 * census rule's hits name (`@sip/corpus` `census-cells.ts`). `--summary`
 * closes with one line per cell beside the call's current run (`@sip/corpus`
 * `replay-summary.ts`). A request whose knobs state `repeat: N` (`compare …
 * --request --repeat N`) is replayed in N passes under the one job, so
 * `compare … @job:<id>` reads its repeats.
 *
 * ONE prep is pinned for the whole invocation and written on every run row, the
 * cases are materialised from content, and the cells run in this process
 * straight into their own bundles. The pin is a CEILING: each capture is read as
 * the newest cut of itself no later than the prep, so a re-cut landing
 * mid-sweep is recorded rather than invisible.
 *
 * `--request <id>` replays a request on the request's own lane: its cells
 * resolved against the pin, one run row per capture — `full` or `cases` as
 * resolved — and batches of `--batch` CELLS across captures. The request's
 * knobs apply; a knob stated on the line other than its default overrides it.
 * `--dry-run` prints the resolution and runs nothing. A work list
 * (`<capture>`, `--known`, `--all`, `--standing`) is planned off the cut store
 * and runs through the same batches.
 *
 * A request's replay is a JOB: `--job <id>` names the row `job start` made and
 * launched this process for; without it the process makes its own row,
 * `running` under its own pid, so a foreground replay is in the queue too. The
 * hooks bracket it, each run keeps their logs over its own cells' window
 * (`@sip/corpus` `hook-keep.ts`), its lock waits show on the row, and the row
 * closes with the exit code on the way out.
 *
 * A job STOPS on SIGTERM (`job stop`): the phase admits no new cell, waits the
 * grace the stop wrote on the row (else `DEFAULT_GRACE`) for the cells in
 * flight, interrupts what is left, closes its open run rows `interrupted`,
 * gives the SUT and the lane's batch process back; the locks release with the
 * scope; the row closes `interrupted` and the process exits
 * `INTERRUPTED_EXIT`. A second SIGTERM changes nothing; only SIGKILL cuts the
 * wind-down, and `reap` then closes what it left. `--unran` (`job restart
 * --resume`) replays the request's cells no job of it ran, every capture as a
 * `cases` set. `--after <job>` waits for that job to end before the prep is
 * pinned — the cut queued ahead of this replay — with the row saying whom it
 * waits for; a stop while waiting ends it.
 *
 * Runs in PARALLEL with other replays and with a cut — nothing it writes is
 * anybody else's — so several lanes are several invocations at once over one
 * cut. A lane the deployment states exclusive (a host-wide singleton SUT)
 * still takes itself alone, and a second invocation on it waits by name.
 *
 * Every invocation appends a new `run<id>` per capture and records a row,
 * including the ones that break: a capture the pinned prep never cut, or one
 * whose cut died, is an `error` the queue shows; a cut that refused every case
 * is `no-cases`. Silence is never an outcome.
 */
import { afterSweep } from "@sip/corpus/housekeeping/cleanup"
import * as HookKeep from "@sip/corpus/jobs/hook-keep"
import * as Hooks from "@sip/corpus/jobs/hooks"
import * as Job from "@sip/corpus/jobs/job"
import { DEFAULT_GRACE } from "@sip/corpus/jobs/job"
import {
  DEFAULT_BATCH,
  DEFAULT_CONCURRENCY,
  DEFAULT_MEMORY_MAX,
  DEFAULT_STARTS_PER_SECOND
} from "@sip/corpus/knobs"
import { errorsFail, type Options } from "@sip/corpus/options"
import * as Population from "@sip/corpus/population"
import * as ReplayBuild from "@sip/corpus/replay/build"
import { pin, planRequest, planWork } from "@sip/corpus/replay/plan"
import { INTERRUPTED_EXIT, phase, type Replay, report, type Stop } from "@sip/corpus/replay/replay"
import * as CensusCells from "@sip/corpus/report/census-cells"
import * as ReplaySummary from "@sip/corpus/report/replay-summary"
import { passes as passesOf } from "@sip/corpus/requests/compare-request"
import { resolutionLines, unranOnly } from "@sip/corpus/requests/request"
import * as Session from "@sip/corpus/session"
import { label, type Work, workList } from "@sip/corpus/universe"
import { Launcher } from "@sip/corpus"
import { Jobs, Requests } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Deferred from "effect/Deferred"
import * as Effect from "effect/Effect"
import * as Exit from "effect/Exit"
import * as Fiber from "effect/Fiber"
import * as Option from "effect/Option"
import { Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { triageFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { optionsOf, USAGE_TAIL } from "../options.js"
import * as Refusals from "../refusals.js"
import { exit } from "../runner.js"
import * as CaseSelection from "./case-selection.js"
import { createdBy } from "./compare.js"

const usage = (facts: CliFacts): string =>
  `usage: ${facts.invocation("replay")} --request <id> | --from-census <file> --rule R | ${USAGE_TAIL}`

export const flags = (facts: CliFacts) => ({
  ...triageFlags(facts),
  request: IdFlags.flag("request", "request").pipe(
    Flag.withDescription("A replay request to run, on its own lane with its own knobs and passes"),
    Flag.optional
  ),
  job: IdFlags.flag("job", "job").pipe(
    Flag.withDescription(`The job row this process was launched for (${facts.invocation("job")} start)`),
    Flag.optional
  ),
  unran: Flag.boolean("unran").pipe(
    Flag.withDescription("Only the request's cells no job of it ran, every capture as a cases set (a resume)"),
    Flag.withDefault(false)
  ),
  after: IdFlags.flag("after", "job").pipe(
    Flag.withDescription("A job to wait for the end of before the prep is pinned: the cut queued ahead"),
    Flag.optional
  ),
  summary: Flag.boolean("summary").pipe(
    Flag.withDescription("Close with one line per cell: its first failing step beside the call's current run"),
    Flag.withDefault(false)
  ),
  fromCensus: Flag.string("from-census").pipe(
    Flag.withDescription("Replay the cells a census report's --rule hits name (a census.json)"),
    Flag.optional
  ),
  rule: Flag.string("rule").pipe(
    Flag.withDescription("--from-census: the RFC rule whose hits name the cells"),
    Flag.optional
  )
})

type Input = Command.Command.Config.Infer<ReturnType<typeof flags>>

/** A knob as the line states it, or as the request carries it where the line left the default. */
const knob = <A>(stated: A, fallback: A, requested: A): A => (stated === fallback ? requested : stated)

/**
 * The request's lane and knobs as the phase's options. The selection is the
 * request's own: a universe request sweeps and tidies after itself, any other
 * is a named list and pays for no walk of the tree.
 */
const requestOptions = Effect.fn("Replay.requestOptions")(function*(input: Input, row: Requests.Row) {
  const base = yield* optionsOf(input)
  return {
    ...base,
    lane: row.lane,
    selection: {
      ...base.selection,
      universe: row.selection.kind === "universe" ? row.selection.universe : "named",
      positional: [],
      cases: []
    },
    batchSize: knob(input.batch, DEFAULT_BATCH, row.knobs.batchSize),
    concurrency: knob(input.concurrency, DEFAULT_CONCURRENCY, row.knobs.concurrency),
    memoryMax: knob(input.memoryMax, DEFAULT_MEMORY_MAX, row.knobs.memoryMax),
    startsPerSecond: knob(input.startsPerSecond, DEFAULT_STARTS_PER_SECOND, row.knobs.startsPerSecond),
    cleanup: input.cleanup && row.knobs.cleanup
  } satisfies Options
})

const noPrep = (facts: CliFacts, named: number | undefined): string =>
  named === undefined
    ? `[replay] refused: the cut store holds no cut — run \`${facts.invocation("cut")}\` first`
    : `[replay] refused: there is no prep ${named} to replay`


/** Every cell the resolution made of the request, gone ones included: what `job_cells` will count. */
const cellsOf = (resolution: Requests.Resolution): number =>
  resolution.captures.reduce((sum, one) => sum + one.planned.length + one.gone.length, 0)

/**
 * The job row this replay runs as: the one named on the line, checked to
 * exist, else one made now for this foreground process.
 */
const jobOf = Effect.fn("Replay.job")(function*(input: Input, row: Requests.Row, options: Options) {
  const jobs = yield* Jobs.Service
  if (Option.isSome(input.job)) {
    const shown = yield* jobs.show(input.job.value)
    return shown === null ? undefined : shown.job.id
  }
  const id = yield* jobs.create({
    kind: "replay",
    requestId: row.id,
    lane: row.lane,
    args: { batch: options.batchSize, concurrency: options.concurrency, memoryMax: options.memoryMax },
    createdBy: yield* createdBy
  })
  yield* jobs.started(id, { pid: process.pid, hooksPath: Job.hooksPath(options.paths.sessions, id) })
  return id
})

const ENDED: ReadonlySet<Jobs.Status> = new Set<Jobs.Status>(["done", "failed", "interrupted", "died"])

const AFTER_POLL_MS = 2_000

/**
 * The end of the job queued ahead, waited for on the row: `waiting_on` names
 * it while this job waits and is cleared once it has ended. A job that does
 * not exist is nothing to wait for. Answers the status it ended with.
 */
const awaitJob = Effect.fn("Replay.awaitJob")(function*(ahead: number, own: number | undefined) {
  const jobs = yield* Jobs.Service
  let told = false
  for (;;) {
    const shown = yield* jobs.show(ahead)
    if (shown === null) return null
    if (ENDED.has(shown.job.status)) {
      if (told && own !== undefined) yield* jobs.waiting(own, null)
      return shown.job.status
    }
    if (!told) {
      told = true
      yield* Console.log(`[replay] waiting for job ${ahead} (${shown.job.kind}) to end`)
      if (own !== undefined) yield* jobs.waiting(own, `job ${ahead} (${shown.job.kind})`)
    }
    yield* Effect.sleep(AFTER_POLL_MS)
  }
})

/** The exit code a request's job is closed with, and the status it means. */
const closing = (code: number): Jobs.CloseRequest =>
  code === INTERRUPTED_EXIT
    ? { status: "interrupted", exit: code }
    : { status: code === 0 ? "done" : "failed", exit: code }

/** The grace a stop wrote on the row, else the default: read when the stop lands, never before. */
const graceOf = Effect.fn("Replay.grace")(function*(jobId: number) {
  const jobs = yield* Jobs.Service
  const shown = yield* jobs.show(jobId).pipe(Effect.catch(() => Effect.succeed(null)))
  return shown?.job.stopGrace ?? DEFAULT_GRACE
})

/**
 * A request replayed as a job: resolved against the pin, bracketed by the
 * hooks, the phase under the lane's locks with the row saying whom it waits
 * for, the row closed on the way out — with the exit code, or `failed` when
 * the process failed before it had one.
 *
 * The whole of it runs MASKED: a SIGTERM (the runtime interrupts the main
 * fiber) lands in one of two places — the wait for the lane, which simply
 * ends, or the wait on the phase, which asks it to stop and waits for its
 * answer — and the closing, the hooks and the row are written whatever came
 * in. The fiber ends interrupted once the mask is left, so the process exits
 * `INTERRUPTED_EXIT` as the row says.
 */
const requestMain = Effect.fn("Replay.request")(function*(facts: CliFacts, input: Input, id: number) {
  return yield* Effect.uninterruptibleMask((restore) => requestMasked(facts, input, id, restore))
})

const requestMasked = Effect.fn("Replay.requestMasked")(function*(
  facts: CliFacts,
  input: Input,
  id: number,
  restore: <A, E, R>(effect: Effect.Effect<A, E, R>) => Effect.Effect<A, E, R>
) {
  const requests = yield* Requests.Service
  const jobs = yield* Jobs.Service
  const shown = yield* requests.show(id)
  if (shown === null) {
    yield* Console.error(`[replay] refused: there is no request ${id}`)
    return 2
  }
  const options = yield* requestOptions(input, shown.request)
  const unstated = yield* Refusals.lane(options.lane)
  if (unstated !== null) {
    yield* Console.error(`[replay] refused: ${unstated}`)
    return 2
  }
  const away = yield* Refusals.environment(facts, options.lane, options.selection.universe)
  if (away !== null) {
    yield* Console.error(`[replay] refused: ${away}`)
    return 2
  }
  if (Option.isSome(input.after)) {
    const ahead = yield* restore(awaitJob(input.after.value, Option.getOrUndefined(input.job)))
    if (ahead !== null) yield* Console.log(`[replay] job ${input.after.value} ${ahead}; resolving now`)
  }
  const prep = yield* pin(options.prep)
  if (prep === null) {
    yield* Console.error(noPrep(facts, options.prep))
    return 2
  }
  const resolved = yield* requests.resolve(id, prep)
  if (resolved === null) {
    yield* Console.error(`[replay] refused: there is no request ${id}`)
    return 2
  }
  // A resume replays what no job ran, every capture as a cases set.
  const resolution = input.unran ? unranOnly(resolved, yield* requests.unran(id)) : resolved
  if (input.unran) {
    const left = resolution.captures.reduce((n, capture) => n + capture.planned.length, 0)
    yield* Console.log(`[replay] resume: ${left} cell(s) of request ${id} no job ran, as cases sets`)
  }
  for (const line of resolutionLines(resolution)) yield* Console.log(line)
  if (options.dryRun) return 0
  const jobId = yield* jobOf(input, shown.request, options)
  if (jobId === undefined) {
    yield* Console.error(`[replay] refused: there is no job ${Option.getOrThrow(input.job)}`)
    return 2
  }
  // The row states its cells before the lane is waited for: `job_cells` are written under the lane.
  yield* jobs.planned(jobId, cellsOf(resolution))
  const launcher = yield* Launcher.Service
  const hooks = yield* launcher.hooks
  const hooksDir = Job.hooksPath(options.paths.sessions, jobId)
  const env: Hooks.HookEnv = {
    job: jobId,
    jobDir: Job.jobDir(options.paths.sessions, jobId),
    captures: resolution.captures.map((capture) => capture.capture),
    lane: options.lane,
    startedAt: new Date().toISOString()
  }
  yield* Hooks.run(hooks, "before", env, hooksDir, launcher.hooksCwd)
  const watch = (holder: string | undefined) => jobs.waiting(jobId, holder ?? null).pipe(Effect.ignore)
  const stop: Stop = yield* Deferred.make<number>()
  const passes = passesOf(shown.request.knobs)
  /** One pass over the request's cells; the first records them on the job, every one marks them ran. */
  const pass = (at: number) =>
    Effect.gen(function*() {
      if (passes > 1) yield* Console.log(`[replay] job ${jobId}: pass ${at + 1} of ${passes}`)
      const captures = yield* planRequest(resolution)
      const work = yield* Effect.forkChild(
        phase({ captures, options, prepId: prep, job: { id: jobId, resolution, recorded: at > 0 }, stop })
      )
      const first = yield* Effect.exit(restore(Fiber.join(work)))
      if (Exit.isSuccess(first)) return first.value
      if (!Cause.hasInterruptsOnly(first.cause)) return yield* Effect.failCause(first.cause)
      // The signal came in while the phase ran: it stops on request and answers.
      const grace = yield* graceOf(jobId)
      yield* Console.error(`[replay] job ${jobId}: stop requested (grace ${grace} s)`)
      yield* Deferred.succeed(stop, grace)
      return yield* Fiber.join(work)
    })
  const ran = yield* Effect.exit(
    Effect.scoped(Effect.gen(function*() {
      yield* restore(ReplayBuild.session(options, `job ${jobId}: replaying request ${id} on ${options.lane}`, watch))
      const done: Array<Replay> = []
      for (let at = 0; at < passes; at++) {
        const one = yield* pass(at)
        done.push(one)
        if (one.interrupted) break
      }
      return done as ReadonlyArray<Replay>
    })).pipe(Effect.catchTag("Session.Held", (held) => Effect.succeed(held)))
  )
  if (Exit.isFailure(ran)) {
    // A stop that came in while the lane was waited for: nothing ran, the row says so.
    if (Cause.hasInterruptsOnly(ran.cause)) {
      yield* Console.error(`[replay] job ${jobId}: stopped before the lane was taken`)
      yield* jobs.close(jobId, closing(INTERRUPTED_EXIT))
      return INTERRUPTED_EXIT
    }
    yield* jobs.close(jobId, { status: "failed", exit: 1 })
    return yield* Effect.failCause(ran.cause)
  }
  const replays = ran.value
  const code = replays instanceof Session.Held ? 2 : yield* closingCode(replays)
  if (replays instanceof Session.Held) yield* Console.error(`[replay] refused: ${replays.held}`)
  const finishedAt = new Date().toISOString()
  const bundles = replays instanceof Session.Held ? [] : replays.flatMap((one) => one.bundles)
  yield* Hooks.run(
    hooks,
    "after",
    { ...env, finishedAt, ...(bundles.length === 1 ? { runDir: bundles[0] } : {}) },
    hooksDir,
    launcher.hooksCwd
  )
  // Each real-lane run keeps the hook logs over its own cells' window.
  const kept = yield* HookKeep.keep(hooksDir, bundles)
  if (kept > 0) yield* Console.log(`[replay] job ${jobId}: ${kept} hook log(s) kept with the runs they cover`)
  yield* jobs.close(jobId, closing(code))
  if (replays instanceof Session.Held) return code
  if (input.summary) for (const line of yield* ReplaySummary.read(`@job:${jobId}`, options.lane)) yield* Console.log(line)
  // After the lane is given back, never under it: a sweep must not be what the
  // next one queues behind while it tidies.
  yield* afterSweep(options, replays.at(-1)?.outcomes.length ?? 0)
  return code
})

/** The exit code of a job's passes: a stop wins, else the worst pass's closing code. */
const closingCode = Effect.fn("ReplayCli.closingCode")(function*(replays: ReadonlyArray<Replay>) {
  const codes = yield* Effect.forEach(replays, (one) => reported(one))
  return codes.includes(INTERRUPTED_EXIT) ? INTERRUPTED_EXIT : Math.max(0, ...codes)
})

/** The closing table and its code, and where the viewer studies the captures. */
const reported = (replay: Replay, options: { readonly errorsFail?: boolean } = {}) =>
  Effect.tap(report(replay, options), () =>
    Console.log("[replay] study any of them at /triage/<capture> in the flow viewer"))

/**
 * Why `--from-census` cannot be honoured on this line, or `null`: it names
 * the cells itself, so it takes no capture, universe or `--case`, and it needs
 * the rule whose hits name them.
 */
const censusRefusal = (input: Input): string | null => {
  if (Option.isNone(input.rule)) return "--from-census needs --rule R: the rule whose hits name the cells"
  if (input.captures.length > 0 || input.cases.length > 0 || input.known || input.all || input.standing) {
    return "--from-census names the cells itself: it takes no capture, --case, --known, --all or --standing"
  }
  return null
}

/** The work a census rule's hits name as of the pin, or why there is none; the hits no case holds are said. */
const censusWork = Effect.fn("Replay.censusWork")(function*(input: Input, prep: number) {
  const refused = censusRefusal(input)
  if (refused !== null) return refused
  const read = yield* CensusCells.read(Option.getOrThrow(input.fromCensus), Option.getOrThrow(input.rule), prep)
  if (typeof read === "string") return read
  for (const line of CensusCells.unmatchedLines(read.unmatched)) yield* Console.error(`[replay] census hits not replayed: ${line}`)
  if (read.work.length === 0) return "no hit of the rule lands on a case of a standing cut"
  return read.work.map((one): Work => ({ capture: one.capture, set: { kind: "cases", cases: one.cases } }))
})

/** What a work list's line names: the census rule's cells, or the selection with its cases read. */
const workOf = Effect.fn("Replay.workOf")(function*(facts: CliFacts, input: Input, options: Options) {
  if (Option.isSome(input.fromCensus)) {
    const prep = yield* pin(options.prep)
    if (prep === null) return noPrep(facts, options.prep)
    const work = yield* censusWork(input, prep)
    if (typeof work === "string") return `[replay] refused: ${work}`
    return { options: { ...options, prep, selection: { ...options.selection, positional: work.map((one) => one.capture) } }, work }
  }
  const read = yield* CaseSelection.resolve(options.selection, facts.refusalWhy)
  if (read._tag === "refused") return `[replay] refused: ${read.why}`
  const selected = { ...options, selection: read.selection }
  const refused = yield* Population.refusal(selected.paths, selected.selection)
  if (refused !== null) return `[replay] refused: ${refused}`
  const work = yield* workList(selected.paths, selected.lane, selected.selection, selected.prep)
  return { options: selected, work }
})

/** A work list replayed: planned off the cut store, then the phase under the lane's locks. */
const workMain = Effect.fn("Replay.work")(function*(facts: CliFacts, input: Input) {
  const unstated = yield* Refusals.lane(input.lane)
  if (unstated !== null) {
    yield* Console.error(`[replay] refused: ${unstated}`)
    return 2
  }
  const stated = yield* optionsOf(input)
  const away = yield* Refusals.environment(facts, stated.lane, stated.selection.universe)
  if (away !== null) {
    yield* Console.error(`[replay] refused: ${away}`)
    return 2
  }
  const asked = yield* workOf(facts, input, stated)
  if (typeof asked === "string") {
    yield* Console.error(asked)
    return 2
  }
  const { options, work: captures } = asked
  if (captures.length === 0) {
    yield* Console.error(options.selection.universe === "named" ? usage(facts) : "[replay] nothing to do")
    return options.selection.universe === "named" ? 2 : 0
  }
  if (options.dryRun) {
    for (const work of captures) yield* Console.log(label(work))
    return 0
  }
  // Pinned before the lane: which cut this invocation answers is settled once,
  // for every capture, and a store with no cut to read is a refusal not a sweep.
  const prep = yield* pin(options.prep)
  if (prep === null) {
    yield* Console.error(noPrep(facts, options.prep))
    return 2
  }
  const replay = yield* Effect.scoped(Effect.gen(function*() {
    yield* ReplayBuild.session(options, `replaying ${captures.length} capture(s) on ${options.lane}`)
    const plans = yield* planWork(captures, prep, options.concurrency)
    return yield* phase({ captures: plans, options, prepId: prep })
  })).pipe(
    Effect.catchTag("Session.Held", (held) => Effect.succeed(held.held)),
    Effect.catchTag("ReplayDb.UnknownCases", (unknown) => Effect.succeed(unknown.message))
  )
  if (typeof replay === "string") {
    yield* Console.error(`[replay] refused: ${replay}`)
    return 2
  }
  const code = yield* reported(replay, { errorsFail: errorsFail(options.selection.universe, input.strict) })
  if (input.summary) for (const line of yield* ReplaySummary.read(`@sweep:${replay.sweep}`, options.lane)) yield* Console.log(line)
  yield* afterSweep(options, replay.outcomes.length)
  return code
})


const main = Effect.fn("Replay.main")(function*(facts: CliFacts, input: Input) {
  return Option.isNone(input.request)
    ? yield* workMain(facts, input)
    : yield* requestMain(facts, input, input.request.value)
})

export const command = (facts: CliFacts) =>
  Command.make("replay", flags(facts), (input) => main(facts, input).pipe(Effect.flatMap(exit))).pipe(
    Command.withDescription(
      `The replay alone: a standing cut or a request driven against one lane\n\n${facts.frontDoors}`
    )
  )
