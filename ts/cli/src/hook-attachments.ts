/**
 * A job's hook logs as its attachments: each a `log` attachment of the job
 * (`Hooks.attachmentsOf`), listed in the job's directory and added to the
 * store under the job. One that cannot be listed is said on stderr and never
 * fails the job.
 */
import * as Hooks from "@sip/corpus/jobs/hooks"
import { Attachments } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"

/** `logs`, the hook logs job `jobId` ran into `jobDir`, attached to the job; nothing where there are none. */
export const attachHooks = Effect.fn("ReplayCli.attachHooks")(function*(
  jobId: number,
  jobDir: string,
  logs: ReadonlyArray<Hooks.HookLog>
) {
  if (logs.length === 0) return
  const store = yield* Attachments.Service
  const listed = Hooks.attachmentsOf(jobDir, logs)
  yield* Effect.forEach(listed, (one) => Attachments.attach(jobDir, one)).pipe(
    Effect.andThen(store.add({ scope: "job", jobId }, { dir: jobDir, attachments: listed })),
    Effect.catch((cause) => Console.error(`[replay] job ${jobId}: hook logs not attached: ${String(cause)}`))
  )
})
