/**
 * The retention pass, composed: rows nobody will close, then what the store
 * still stands behind, then the run bundles, cut logs and cache entries the
 * retention ruling drops (`retention.ts`).
 *
 * The order is the dependency order. Reaping first — dead jobs, then rows
 * nobody will close — makes `finished_at IS NULL` honest, and everything
 * behind it reads that column — the floor a live command pins its cut at is
 * the lowest prep an open run names. A pass that ran them the other way round
 * would be as conservative as the last killed sweep left it.
 *
 * It takes no lock. Every run bundle is named by the store, every cache entry
 * by its content and published by a rename, and what the store stands behind
 * is read from the same floor a live command reads its cut at — so a pass that
 * runs beside a cut, a replay or another pass can only name things nobody is
 * writing.
 *
 * It runs at the end of a SWEEP unless the sweep turned it off, and nowhere
 * else ({@link afterSweep}). Whatever it fails on is logged and dropped: the
 * next sweep runs it again, and nothing here is urgent enough to change what a
 * sweep reports. The cache pass alone ({@link collectContent}) is what a
 * retire or a delete runs behind its own removal: the rows just went, so what
 * they alone cited is collectable now.
 */
import { Cuts, Runs } from "@sip/store"
import * as Cause from "effect/Cause"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { CONTENT } from "../content/content.js"
import * as ContentGc from "../content/content-gc.js"
import { LaneHosting } from "../deployment/index.js"
import type { Options } from "../options.js"
import type { Paths } from "../paths.js"
import * as Reap from "./reap.js"
import * as Retention from "./retention.js"

export interface Request {
  readonly paths: Paths
  /** A dry run rules on everything and removes nothing. */
  readonly dryRun: boolean
  /** Whether to recurse what it drops to report the bytes; no ruling needs it. */
  readonly measure: boolean
  readonly horizonHours: number
  /** Hours an unreferenced content object is kept. */
  readonly graceHours: number
  /** Hours a paused-clock bundle no other rule keeps survives its run (`Retention.GRACE_HOURS`). */
  readonly keepHours: number
  /** Whether a bundle no row names goes with the rest: a store rebuilt from nothing. */
  readonly forgetUnrecorded: boolean
}

const mb = (bytes: number): string => `${(bytes / 1024 / 1024).toFixed(1)} MB`

/**
 * The apparent bytes of the files a pass drops, and the warning that goes with
 * the number. It is a sum of file sizes; the filesystem charges whole blocks
 * per file and an inode per directory over a tree of many small files, so what
 * the disk recovers is LARGER than this — the number is what the pass drops.
 */
const measured = Effect.fn("Cleanup.measured")(function*(dirs: ReadonlyArray<string>) {
  let bytes = 0
  for (const dir of dirs) bytes += yield* Retention.sizeOf(dir)
  return `${mb(bytes)} of file bytes — not the disk, which moves more`
})

/**
 * What the store stands behind, from the lowest live ceiling: the standing cut
 * floored by every prep an open run still pins; `null` where it stands behind
 * nothing.
 */
const liveReading = Effect.fn("Cleanup.liveReading")(function*(open: ReadonlyArray<Runs.Unfinished>) {
  const cuts = yield* Cuts.Service
  const standing = yield* cuts.standingPrep
  if (standing === null) return null
  const openPreps = open.flatMap((row) => (row.prepId === null ? [] : [row.prepId]))
  return yield* cuts.contentFrom(Math.min(standing, ...openPreps))
})

/** Every entry removed, whole. */
const removeAll = Effect.fn("Cleanup.removeAll")(function*(entries: ReadonlyArray<string>) {
  const fs = yield* FileSystem.FileSystem
  for (const entry of entries) yield* fs.remove(entry, { recursive: true, force: true })
})

/** The whole pass: rule on everything, then remove what the ruling names. */
export const pass = Effect.fn("Cleanup.pass")(function*(request: Request) {
  const runs = yield* Runs.Service
  const hosting = yield* LaneHosting.Service
  // Jobs first: a job whose process is gone closes its own runs, so the row
  // pass behind it only rules on what no job answers for.
  for (const line of Reap.jobLines(yield* Reap.reapJobs(request.dryRun), request.dryRun)) yield* Console.log(line)
  const reaped = yield* Reap.reap({ horizonHours: request.horizonHours, dryRun: request.dryRun })
  const said = Reap.line(reaped, request.dryRun)
  if (said !== undefined) yield* Console.log(said)

  // What is STILL open, read once and handed to the passes behind the reaping
  // one. A dry run rules against the same answer the real pass would, because
  // the rows it would have closed are taken out here rather than in the table.
  const closed = new Set(reaped.dead)
  const open = (yield* runs.unfinished).filter((row) => !closed.has(row.id))

  // What the store stands behind, read ONCE from the lowest live ceiling. It
  // answers both rulings behind it — which cut logs are still somebody's, and
  // which cache entries a live reading can still name.
  const live = yield* liveReading(open)

  const plan = yield* Retention.plan({
    sessions: request.paths.sessions,
    livePreps: live === null ? null : live.preps,
    realLanes: yield* LaneHosting.realClock(hosting),
    graceHours: request.keepHours,
    forgetUnrecorded: request.forgetUnrecorded
  })
  const drops = Retention.dropped(plan)
  yield* Console.log(
    `[cleanup] ${plan.onDisk.length} bundle(s) under ${request.paths.sessions}: ` +
      `keep=${plan.onDisk.length - plan.removable.length - (request.forgetUnrecorded ? 0 : plan.unrecorded.length)} ` +
      `unrecorded=${plan.unrecorded.length} removable=${plan.removable.length} ` +
      `stale-preps=${plan.preps.length}` +
      (request.measure ? ` (${yield* measured(drops)})` : "")
  )
  if (plan.unrecorded.length > 0 && !request.forgetUnrecorded) {
    yield* Console.log(
      `[cleanup] leaving ${plan.unrecorded.length} bundle(s) the database has never seen — ` +
        "import them into the store first if they are evidence, or forget them after a rebuild"
    )
    for (const dir of plan.unrecorded.slice(0, 10)) yield* Console.log(`   ? ${dir}`)
  }

  const cache = yield* ContentGc.plan({
    sessions: request.paths.sessions,
    graceHours: request.graceHours,
    live,
    anyOpen: open.length > 0
  })
  const cached = ContentGc.collected(cache)
  yield* Console.log(ContentGc.line(cache, request.dryRun))

  if (request.dryRun) {
    for (const entry of [...drops, ...cached]) yield* Console.log(`   - ${entry}`)
    return
  }
  yield* removeAll([...drops, ...cached])
  yield* Console.log(
    `[cleanup] removed ${plan.removable.length} bundle(s), ${plan.preps.length} cut log(s) ` +
      `and ${cached.length} cache entr(ies) — every row survives`
  )
})

export interface ContentRequest {
  readonly sessions: string
  readonly graceHours: number
  readonly dryRun: boolean
}

/**
 * The cache pass alone: the `_content/` entries no live reading cites, past
 * the grace, removed unless dry. Reaps nothing and rules on no bundle — a
 * removal verb has just taken its rows and wants the cache to follow.
 */
export const collectContent = Effect.fn("Cleanup.collectContent")(function*(request: ContentRequest) {
  const runs = yield* Runs.Service
  const open = yield* runs.unfinished
  const cache = yield* ContentGc.plan({
    sessions: request.sessions,
    graceHours: request.graceHours,
    live: yield* liveReading(open),
    anyOpen: open.length > 0
  })
  yield* Console.log(ContentGc.line(cache, request.dryRun))
  if (!request.dryRun) yield* removeAll(ContentGc.collected(cache))
  return cache
})

/** `<sessions>/_content` removed and nothing else: every entry rebuilds from a row on the next read. */
export const rebuildCache = Effect.fn("Cleanup.rebuildCache")(function*(sessions: string) {
  const cache = path.join(sessions, CONTENT)
  yield* removeAll([cache])
  yield* Console.log(`[cleanup] removed ${cache} — every entry rebuilds from a row on the next read`)
})

/**
 * The pass after a SWEEP — an invocation that replayed more than one capture —
 * unless it turned the pass off, and after nothing else.
 *
 * A sweep is already a minutes-to-hours command, so a seconds-long pass on the
 * end of it is noise. A one-capture invocation pays nothing: it produced one
 * bundle, and making a single-capture triage walk the whole tree for it is the
 * failure mode to avoid.
 *
 * It runs after the sweep has given its LANE back, so a lane's next invocation
 * never queues behind a tidy-up, and it never measures. Whatever it fails on
 * is logged and dropped — a retention pass must not change what a sweep
 * reports.
 */
export const afterSweep = Effect.fn("Cleanup.afterSweep")(function*(
  options: Pick<Options, "cleanup" | "paths">,
  captures: number
) {
  if (!options.cleanup || captures < 2) return
  yield* pass({
    paths: options.paths,
    dryRun: false,
    measure: false,
    horizonHours: Reap.HORIZON_HOURS,
    graceHours: ContentGc.GRACE_HOURS,
    keepHours: Retention.GRACE_HOURS,
    forgetUnrecorded: false
  }).pipe(
    Effect.catchCause((cause) => Console.error(`[cleanup] the pass after this sweep did not run: ${Cause.pretty(cause)}`))
  )
})
