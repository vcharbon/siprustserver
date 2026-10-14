/**
 * WHICH run bundles the session tree keeps, and which it may drop, by what
 * remaking them would cost. Every run keeps its ROW forever; a BUNDLE is kept
 * where remaking it is expensive or someone needs it:
 *
 * - a real-clock lane's (`realLanes`): its calls' wall time and a lab;
 * - a call's current run's — every (capture, lane)'s latest and latest green
 *   current run, and each case's current cases run;
 * - a pinned run's;
 * - a cited run's: one a ticket names, by itself, its job or its sweep;
 * - an open run's, which is still being written;
 * - a paused-clock run that finished within `graceHours`: a task
 *   environment's evidence, read back while its work is under way.
 *
 * Every other paused-clock bundle goes: a replay remakes it in seconds.
 *
 * A bundle no row names is NAMED and kept: it may be evidence nobody has
 * imported yet. Where the pass is told the store was rebuilt from nothing
 * (`forgetUnrecorded`), it names nothing anyone will import, and it goes with
 * the rest.
 *
 * A cut's logs (`_preps/prep<N>`) survive while the store still stands behind
 * that cut — while some capture's standing reading, at or above the lowest
 * live ceiling, is that prep's. A cut a command can still be asked to replay
 * is never the wrong cut to hold a generator log for.
 */
import { Runs } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import { PREPS } from "../paths.js"

/**
 * The tree's own children — its caches and its logs — are named with a leading
 * underscore; a capture's session workdir is named after the capture, which
 * never is.
 */
const isCapture = (stem: string): boolean => !stem.startsWith("_")

/** Whether `file` is a directory; false where it cannot be read. */
const isDirectory = (fs: FileSystem.FileSystem, file: string) =>
  fs.stat(file).pipe(Effect.map((info) => info.type === "Directory"), Effect.orElseSucceed(() => false))

/** Every `run<id>` bundle under the session tree's capture workdirs. */
const listRunDirs = Effect.fn("Retention.listRunDirs")(function*(sessions: string) {
  const fs = yield* FileSystem.FileSystem
  const out: Array<string> = []
  const stems = yield* fs.readDirectory(sessions).pipe(Effect.orElseSucceed(() => [] as Array<string>))
  for (const stem of [...stems].sort()) {
    if (!isCapture(stem)) continue
    const sessionDir = path.join(sessions, stem)
    if (!(yield* isDirectory(fs, sessionDir))) continue
    const entries = yield* fs.readDirectory(sessionDir).pipe(Effect.orElseSucceed(() => [] as Array<string>))
    for (const entry of entries) if (/^run\d+$/.test(entry)) out.push(path.join(sessionDir, entry))
  }
  return out as ReadonlyArray<string>
})

/**
 * Cut log directories outside what the store still stands behind. A caller that
 * knows of no standing cut at all names none of them: an empty reading is a
 * store that cannot vouch for anything, not one that vouches for nothing.
 */
export const stalePreps = Effect.fn("Retention.stalePreps")(function*(
  sessions: string,
  live: ReadonlySet<number> | null
) {
  const fs = yield* FileSystem.FileSystem
  const root = path.join(sessions, PREPS)
  if (live === null) return [] as ReadonlyArray<string>
  const entries = yield* fs.readDirectory(root).pipe(Effect.orElseSucceed(() => [] as Array<string>))
  return entries
    .filter((entry) => {
      const digits = /^prep(\d+)$/.exec(entry)?.[1]
      return digits !== undefined && !live.has(Number(digits))
    })
    .map((entry) => path.join(root, entry)) as ReadonlyArray<string>
})

/** What one pass over the tree found, and what it may remove. */
export interface Plan {
  readonly onDisk: ReadonlyArray<string>
  /** Bundles the database has never seen. */
  readonly unrecorded: ReadonlyArray<string>
  readonly removable: ReadonlyArray<string>
  readonly preps: ReadonlyArray<string>
}

/** Everything the plan drops, in the order a removal walks it. */
export const dropped = (plan: Plan): ReadonlyArray<string> => [...plan.removable, ...plan.preps]

/** How long a paused-clock bundle no other rule keeps survives its run's end. */
export const GRACE_HOURS = 24

export interface PlanRequest {
  readonly sessions: string
  /**
   * The preps the store still stands behind, or `null` where it stands behind
   * nothing. It is an INPUT because the caller reads it once, from a floor the
   * reaping pass in front of this one moves.
   */
  readonly livePreps: ReadonlySet<number> | null
  /** The lanes whose bundles are kept whatever else holds. */
  readonly realLanes: ReadonlyArray<string>
  readonly graceHours: number
  /** Whether a bundle no row names goes with the rest. */
  readonly forgetUnrecorded: boolean
  /** The clock the grace is read against; now where absent. */
  readonly now?: number
}

/** The whole retention ruling, read-only: nothing here removes anything. */
export const plan = Effect.fn("Retention.plan")(function*(request: PlanRequest) {
  const runs = yield* Runs.Service
  const current = yield* runs.protectedBundles
  const recorded = new Map((yield* runs.allBundles).map((row) => [row.bundlePath, row]))
  const real = new Set(request.realLanes)
  const since = (request.now ?? Date.now()) - request.graceHours * 3_600_000
  const keeps = (row: Runs.RecordedBundle): boolean =>
    real.has(row.lane) ||
    current.has(row.bundlePath) ||
    row.pinned ||
    row.cited ||
    row.finishedAt === null ||
    Date.parse(row.finishedAt) >= since
  const onDisk = yield* listRunDirs(request.sessions)
  const unrecorded = onDisk.filter((dir) => !recorded.has(dir))
  return {
    onDisk,
    unrecorded,
    removable: onDisk.filter((dir) => {
      const row = recorded.get(dir)
      return row === undefined ? request.forgetUnrecorded : !keeps(row)
    }),
    preps: yield* stalePreps(request.sessions, request.livePreps)
  } satisfies Plan
})

/**
 * The bytes the files under `dir` declare. Answered only when a caller asks for
 * it: recursing every removable directory is the most expensive thing this pass
 * can do, and no ruling depends on the number.
 */
export const sizeOf = (dir: string): Effect.Effect<number, never, FileSystem.FileSystem> =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    let total = 0
    const entries = yield* fs.readDirectory(dir).pipe(Effect.orElseSucceed(() => [] as Array<string>))
    for (const entry of entries) {
      const child = path.join(dir, entry)
      const info = yield* fs.stat(child).pipe(Effect.orElseSucceed(() => undefined))
      if (info === undefined) continue
      if (info.type === "Directory") total += yield* sizeOf(child)
      else if (info.type === "File") total += Number(info.size)
    }
    return total
  })
