/**
 * `retire` — sources taken out of the corpus:
 *
 *   retire <capture>... [--yes] [--measure] [--grace HOURS]
 *   retire --job <id> --yes <capture>...       (what a launched retire job runs)
 *
 * It prints first what goes — the rows per table, the runs and their bundles,
 * the session and artifact directories, the upload, the bytes under
 * `--measure` — and removes nothing without `--yes`. With it, the command
 * runs as a `retire` job: the one named by `--job`, else a row of its own
 * under this pid; the rows go in one transaction (`@sip/corpus` `retire.ts`),
 * the files after, then the cache pass. Refused, with nothing removed and the
 * job `failed`, while a running job holds one of the captures.
 */
import { GRACE_HOURS } from "@sip/corpus/content/content-gc"
import * as Retire from "@sip/corpus/ingest/retire"
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
  captures: Argument.string("capture").pipe(
    Argument.withDescription("A capture name, extension and all"),
    Argument.variadic()
  ),
  yes: Flag.boolean("yes").pipe(Flag.withDescription("Remove it; without this, only say what would go"), Flag.withDefault(false)),
  measure: Flag.boolean("measure").pipe(
    Flag.withDescription("Recurse what goes to report the bytes"),
    Flag.withDefault(false)
  ),
  grace: Flag.float("grace").pipe(
    Flag.withDescription("Hours an unreferenced content object is kept by the cache pass after"),
    Flag.withDefault(GRACE_HOURS)
  ),
  job: IdFlags.flag("job", "job").pipe(
    Flag.withDescription(`The job row this process was launched for (${facts.invocation("job")} start retire)`),
    Flag.optional
  )
})

export const command = (facts: CliFacts) =>
  Command.make("retire", flags(facts), (input) =>
    Effect.gen(function*() {
      const paths = yield* pathsOf(overridesOf(input))
      const captures = [...new Set(input.captures)]
      if (captures.length === 0) {
        yield* Console.error("[retire] refused: no capture named")
        return yield* exit(2)
      }
      const plans = yield* Retire.plan({ paths, captures, measure: input.measure })
      for (const one of plans) for (const line of Retire.lines(one)) yield* Console.log(line)
      const held = plans.filter((one) => one.holders.length > 0)
      const unknown = plans.filter((one) => !one.known)
      if (!input.yes) {
        yield* Console.log(
          held.length > 0
            ? `[retire] nothing removed: ${held.map((one) => one.capture).join(", ")} held by a running job`
            : `[retire] nothing removed — say --yes to remove ${plans.length - unknown.length} capture(s)`
        )
        return
      }
      if (held.length > 0) {
        yield* Console.error(`[retire] refused: ${held.map((one) => one.capture).join(", ")} held by a running job`)
        return yield* exit(2)
      }
      const job = yield* JobStart.runningAs(
        Option.getOrUndefined(input.job),
        "retire",
        { captures, measure: input.measure },
        paths.sessions
      ).pipe(Effect.catchIf(JobStart.isNoSuchJob, (refused) => Effect.succeed(refused)))
      if (job instanceof JobStart.NoSuchJob) {
        yield* Console.error(`[retire] refused: ${job.message}`)
        return yield* exit(2)
      }
      const rows = yield* Jobs.Service
      yield* rows.planned(job, captures.length)
      const applied = yield* Retire.apply({ paths, captures, measure: input.measure, graceHours: input.grace }).pipe(
        Effect.catchIf(Removal.isHeld, (refused) => Effect.succeed(refused)),
        Effect.tapCause(() => rows.close(job, { status: "failed", exit: 1 }).pipe(Effect.ignore))
      )
      if (applied instanceof Removal.Held) {
        yield* Console.error(`[retire] refused: ${applied.message}`)
        yield* rows.close(job, { status: "failed", exit: 2 })
        return yield* exit(2)
      }
      const bundles = applied.plans.reduce((sum, one) => sum + one.bundles.length, 0)
      const runs = applied.plans.reduce((sum, one) => sum + one.rows.runs, 0)
      yield* Console.log(
        `[retire] job ${job}: ${applied.plans.filter((one) => one.known).length} capture(s) retired — ` +
          `${runs} run row(s), ${bundles} bundle(s), ` +
          `${applied.plans.filter((one) => one.artifactDir !== null).length} artifact director(ies), ` +
          `${applied.plans.reduce((sum, one) => sum + one.uploads.length, 0)} upload(s)`
      )
      yield* rows.close(job, { status: "done", exit: 0 })
    })).pipe(Command.withDescription("Retire sources: every row and file a capture produced, then the cache pass"))
