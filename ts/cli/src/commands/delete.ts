/**
 * `delete` — runs removed, rows and bundles together:
 *
 *   delete <run id>... [--yes] [--measure] [--grace HOURS]
 *   delete --request <id> | --sweep <id> | --of-job <id> | --lane L --older-than HOURS [--yes] [--measure]
 *   delete --job <id> --yes …                  (what a launched delete job runs)
 *
 * One selector per line: run ids, the runs the jobs of a request opened, a
 * sweep's runs, the runs one job opened, or the runs finished more than HOURS
 * ago on a lane. It prints first what goes — each run and its bundle where
 * one is still on disk, the rows per table, the bytes under `--measure` — and
 * removes nothing without `--yes`. With it, the command runs as a `delete`
 * job: the one named by `--job`, else a row of its own under this pid; the
 * rows go in one transaction (`@sip/corpus` `delete-runs.ts`), the bundles
 * after, then the cache pass. Refused, with nothing removed, while a selected
 * run's job runs or the run is still open.
 */
import { GRACE_HOURS } from "@sip/corpus/content/content-gc"
import * as Delete from "@sip/corpus/housekeeping/delete-runs"
import * as JobStart from "@sip/corpus/jobs/job-start"
import { Jobs, Removal } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { Argument, Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { pathFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { overridesOf, pathsOf } from "../options.js"
import { exit } from "../runner.js"

export const flags = (facts: CliFacts) => ({
  ...pathFlags,
  ids: IdFlags.argument("run", "run").pipe(Argument.withDescription("A run id (N, runN, @run:N)"), Argument.variadic()),
  request: IdFlags.flag("request", "request").pipe(
    Flag.withDescription("Every run the jobs of this request opened"),
    Flag.optional
  ),
  sweep: IdFlags.flag("sweep", "sweep").pipe(Flag.withDescription("Every run of this sweep"), Flag.optional),
  ofJob: IdFlags.flag("of-job", "job").pipe(Flag.withDescription("Every run this job opened"), Flag.optional),
  lane: Flag.string("lane").pipe(Flag.withDescription("With --older-than: the lane"), Flag.optional),
  olderThan: Flag.float("older-than").pipe(
    Flag.withDescription("With --lane: every run finished more than this many hours ago"),
    Flag.optional
  ),
  yes: Flag.boolean("yes").pipe(Flag.withDescription("Remove it; without this, only say what would go"), Flag.withDefault(false)),
  measure: Flag.boolean("measure").pipe(Flag.withDescription("Recurse the bundles to report the bytes"), Flag.withDefault(false)),
  grace: Flag.float("grace").pipe(
    Flag.withDescription("Hours an unreferenced content object is kept by the cache pass after"),
    Flag.withDefault(GRACE_HOURS)
  ),
  job: IdFlags.flag("job", "job").pipe(
    Flag.withDescription(`The job row this process was launched for (${facts.invocation("job")} start delete)`),
    Flag.optional
  )
})

type Input = Command.Command.Config.Infer<ReturnType<typeof flags>>

const what = (selector: Removal.Selector): string => {
  switch (selector._tag) {
    case "ids":
      return selector.ids.map((id) => `run${id}`).join(", ")
    case "request":
      return `the runs of request ${selector.requestId}`
    case "sweep":
      return `the runs of sweep ${selector.sweepId}`
    case "job":
      return `the runs of job ${selector.jobId}`
    case "older":
      return `the runs on ${selector.lane} older than ${selector.hours} h`
  }
}

const run = (input: Input) =>
  Effect.gen(function*() {
    const paths = yield* pathsOf(overridesOf(input))
    const selector = Delete.selectorOf(input)
    if (typeof selector === "string") {
      yield* Console.error(`[delete] refused: ${selector}`)
      return yield* exit(2)
    }
    const planned = yield* Delete.plan({ paths, selector, measure: input.measure })
    yield* Console.log(`[delete] ${what(selector)}: ${planned.runs.length} run(s) match`)
    for (const line of Delete.lines(planned)) yield* Console.log(line)
    if (!input.yes) {
      yield* Console.log(
        planned.held.length > 0
          ? `[delete] nothing removed: ${planned.held.length} run(s) held`
          : `[delete] nothing removed — say --yes to remove ${planned.runs.length} run(s)`
      )
      return
    }
    if (planned.held.length > 0) {
      yield* Console.error(`[delete] refused: ${planned.held.map((one) => `run${one.id}: ${one.why}`).join("; ")}`)
      return yield* exit(2)
    }
    const jobs = yield* Jobs.Service
    if (planned.runs.length === 0) {
      yield* Console.log("[delete] nothing to remove")
      // A launched job with nothing to do still ends: its row closes done over nothing.
      if (Option.isSome(input.job)) {
        yield* jobs.planned(input.job.value, 0)
        yield* jobs.close(input.job.value, { status: "done", exit: 0 })
      }
      return
    }
    const job = yield* JobStart.runningAs(
      Option.getOrUndefined(input.job),
      "delete",
      { selector, measure: input.measure },
      paths.sessions
    ).pipe(Effect.catchIf(JobStart.isNoSuchJob, (refused) => Effect.succeed(refused)))
    if (job instanceof JobStart.NoSuchJob) {
      yield* Console.error(`[delete] refused: ${job.message}`)
      return yield* exit(2)
    }
    yield* jobs.planned(job, planned.runs.length)
    const applied = yield* Delete.apply({ paths, selector, measure: input.measure, graceHours: input.grace }).pipe(
      Effect.catchIf(Removal.isHeld, (refused) => Effect.succeed(refused)),
      Effect.tapCause(() => jobs.close(job, { status: "failed", exit: 1 }).pipe(Effect.ignore))
    )
    if (applied instanceof Removal.Held) {
      yield* Console.error(`[delete] refused: ${applied.message}`)
      yield* jobs.close(job, { status: "failed", exit: 2 })
      return yield* exit(2)
    }
    yield* Console.log(
      `[delete] job ${job}: ${applied.plan.rows.runs} run row(s) removed with ` +
        `${applied.plan.rows.caseResults} case result(s) and ${applied.plan.rows.runRuleHits} rule hit(s), ` +
        `${applied.plan.bundles.length} bundle(s) off disk`
    )
    yield* jobs.close(job, { status: "done", exit: 0 })
  })

export const command = (facts: CliFacts) =>
  Command.make("delete", flags(facts), run).pipe(
    Command.withDescription("Delete runs: the rows and the bundles together, then the cache pass")
  )

