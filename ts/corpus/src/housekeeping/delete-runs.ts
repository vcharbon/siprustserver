/**
 * DELETING runs: the rows and the bundles together, hard, then the cache GC
 * behind them.
 *
 * A delete names its runs by id, by the request whose jobs opened them, by
 * the job that opened them, by their sweep, or by age on a lane
 * (`Removal.Selector`). The plan reads the store and the disk: the rows per
 * table, each run's bundle where it still exists — the retention pass may have
 * dropped it already, which changes nothing about the row going — and the
 * runs that may not go now because their job runs or nothing has closed them.
 *
 * Apply takes the rows in ONE transaction, refused where any selected run is
 * held; the bundles go after the commit, and a crash between the two leaves
 * bundles the next retention pass names as unrecorded and keeps. The standing
 * of a (capture, lane) is a query, so the previous current run stands the
 * moment the row is gone, nothing recomputed. Then the cache pass runs: a
 * delete frees no case content — cuts cite it, not runs — but a merged side-row
 * directory under `_ledgers/` may have been the deleted run's alone.
 */
import { Removal } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Option from "effect/Option"
import type * as ContentGc from "../content/content-gc.js"
import type { Paths } from "../paths.js"
import * as Cleanup from "./cleanup.js"
import { sizeOf } from "./retention.js"

/** What a delete line may state, each optional. */
export interface SelectorInput {
  readonly ids: ReadonlyArray<number>
  readonly request: Option.Option<number>
  readonly sweep: Option.Option<number>
  readonly ofJob: Option.Option<number>
  readonly lane: Option.Option<string>
  readonly olderThan: Option.Option<number>
}

/** The one selector the line states, or why it states none or several. */
export const selectorOf = (input: SelectorInput): Removal.Selector | string => {
  const stated: Array<Removal.Selector> = []
  if (input.ids.length > 0) stated.push({ _tag: "ids", ids: [...new Set(input.ids)] })
  if (Option.isSome(input.request)) stated.push({ _tag: "request", requestId: input.request.value })
  if (Option.isSome(input.sweep)) stated.push({ _tag: "sweep", sweepId: input.sweep.value })
  if (Option.isSome(input.ofJob)) stated.push({ _tag: "job", jobId: input.ofJob.value })
  if (Option.isSome(input.lane) || Option.isSome(input.olderThan)) {
    if (Option.isNone(input.lane) || Option.isNone(input.olderThan)) return "--lane and --older-than go together"
    stated.push({ _tag: "older", lane: input.lane.value, hours: input.olderThan.value })
  }
  if (stated.length === 0) {
    return "name run ids, or --request <id>, --sweep <id>, --of-job <id>, or --lane L --older-than HOURS"
  }
  if (stated.length > 1) return "one selector per line: run ids, --request, --sweep, --of-job, or --lane with --older-than"
  return stated[0]!
}

export interface Plan extends Removal.DeletePlan {
  /** The bundles still on disk, absolute. */
  readonly bundles: ReadonlyArray<string>
  /** The file bytes of the bundles, where measured; not the disk, which moves more. */
  readonly bytes: number | null
}

export interface PlanRequest {
  readonly paths: Paths
  readonly selector: Removal.Selector
  readonly measure: boolean
}

/** What deleting the selected runs would take, read-only. */
export const plan = Effect.fn("Delete.plan")(function*(request: PlanRequest) {
  const removal = yield* Removal.Service
  const fs = yield* FileSystem.FileSystem
  const store = yield* removal.deletePlan(request.selector)
  const bundles: Array<string> = []
  for (const run of store.runs) {
    if (yield* fs.exists(run.bundlePath).pipe(Effect.orElseSucceed(() => false))) bundles.push(run.bundlePath)
  }
  let bytes: number | null = null
  if (request.measure) {
    bytes = 0
    for (const bundle of bundles) bytes += yield* sizeOf(bundle)
  }
  return { ...store, bundles, bytes } satisfies Plan
})

export interface ApplyRequest extends PlanRequest {
  readonly graceHours: number
}

export interface Applied {
  readonly plan: Plan
  readonly cache: ContentGc.Collected
}

/**
 * The rows in one transaction, then the bundles, then the cache pass. Fails
 * `Removal.Held` — nothing removed — where a selected run may not go.
 */
export const apply = Effect.fn("Delete.apply")(function*(request: ApplyRequest) {
  const removal = yield* Removal.Service
  const fs = yield* FileSystem.FileSystem
  const planned = yield* plan(request)
  yield* removal.deleteRuns(planned.runs.map((run) => run.id))
  for (const bundle of planned.bundles) yield* fs.remove(bundle, { recursive: true, force: true })
  const cache = yield* Cleanup.collectContent({
    sessions: request.paths.sessions,
    graceHours: request.graceHours,
    dryRun: false
  })
  return { plan: planned, cache } satisfies Applied
})

const mb = (bytes: number): string => `${(bytes / 1024 / 1024).toFixed(1)} MB`

/** What the delete is worth saying: one line per run, the rows, the bytes, the holds. */
export const lines = (one: Plan): ReadonlyArray<string> => {
  if (one.runs.length === 0) return ["no run matches"]
  const onDisk = new Set(one.bundles)
  const out: Array<string> = one.runs.map((run) =>
    `  run${run.id}  ${run.capture}  ${run.lane}  ${run.status}` +
    (run.finishedAt === null ? "  open" : "") +
    (onDisk.has(run.bundlePath) ? `  ${run.bundlePath}` : "  (no bundle on disk)")
  )
  out.push(
    `  rows: ${Removal.DELETE_TABLES.map((table) => `${table} ${one.rows[table]}`).join(", ")}` +
      ` — ${one.bundles.length} bundle(s) on disk`
  )
  if (one.bytes !== null) out.push(`  ${mb(one.bytes)} of file bytes — not the disk, which moves more`)
  for (const held of one.held) out.push(`  HELD run${held.id}: ${held.why}`)
  return out
}
