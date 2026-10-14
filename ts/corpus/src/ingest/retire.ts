/**
 * RETIRING a source: everything a capture produced, in the store and on disk,
 * then the cache collection behind it.
 *
 * The plan reads both stores and names what goes: the rows per table
 * (`Removal.retirePlan`), the run bundles, the session directory
 * `<sessions>/<stem>`, the artifact directory `<artifacts>/<capture>`, and
 * the pcap under `<artifacts>/_uploads` its ingest record names — that one
 * alone: a pcap anywhere else is the host's, never the store's. A capture the
 * store holds nothing of but the disk still does — a retire that crashed
 * between its commit and its removals — plans its directories and nothing
 * else, so running it again finishes the job.
 *
 * Apply takes the rows in ONE transaction, refused where a running job holds
 * the capture; the files go after the commit. A crash between the two leaves
 * the directories on disk: the next retention pass names the bundles as ones
 * the store has never seen and keeps them, and a second retire of the capture
 * removes them. Then the cache pass collects the `_content/` entries the
 * capture alone cited, past the grace as always.
 */
import { Removal } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import type * as ContentGc from "../content/content-gc.js"
import * as Cleanup from "../housekeeping/cleanup.js"
import { sizeOf } from "../housekeeping/retention.js"
import { type Paths, stemOf } from "../paths.js"
import * as Record from "./record.js"
import { UPLOADS } from "./source.js"

/** One capture's plan: the store's rows and runs, the directories and files on disk. */
export interface Plan extends Removal.RetirePlan {
  /** Absolute; each named only where it exists. */
  readonly bundles: ReadonlyArray<string>
  readonly sessionDir: string | null
  readonly artifactDir: string | null
  /** The pcaps under `_uploads` the ingest record names. */
  readonly uploads: ReadonlyArray<string>
  /** Whether the store or the disk holds anything of it. */
  readonly known: boolean
  /** The file bytes of what goes, where measured; not the disk, which moves more. */
  readonly bytes: number | null
}

export interface PlanRequest {
  readonly paths: Paths
  readonly captures: ReadonlyArray<string>
  readonly measure: boolean
}

const under = (file: string, root: string): boolean => {
  const relative = path.relative(root, file)
  return relative !== "" && !relative.startsWith("..") && !path.isAbsolute(relative)
}

/** The pcaps the capture's ingest record names that lie under `_uploads` — followed through links both ways. */
export const uploadsOf = Effect.fn("Retire.uploadsOf")(function*(artifacts: string, capture: string) {
  const fs = yield* FileSystem.FileSystem
  const record = yield* Record.read(artifacts, capture)
  if (record === null) return [] as ReadonlyArray<string>
  const realOrSelf = (one: string) => fs.realPath(one).pipe(Effect.orElseSucceed(() => one))
  const uploads = path.join(artifacts, UPLOADS)
  const realUploads = yield* realOrSelf(uploads)
  const found: Array<string> = []
  for (const one of record.pcaps) {
    const inside = under(one.path, uploads) || under(yield* realOrSelf(one.path), realUploads)
    if (inside && (yield* fs.exists(one.path).pipe(Effect.orElseSucceed(() => false)))) found.push(one.path)
  }
  return found as ReadonlyArray<string>
})

const bytesOf = Effect.fn("Retire.bytesOf")(function*(plan: Omit<Plan, "bytes" | "known">) {
  const fs = yield* FileSystem.FileSystem
  let total = 0
  // The session directory holds the bundles: count it once.
  if (plan.sessionDir !== null) total += yield* sizeOf(plan.sessionDir)
  else for (const bundle of plan.bundles) total += yield* sizeOf(bundle)
  if (plan.artifactDir !== null) total += yield* sizeOf(plan.artifactDir)
  for (const file of plan.uploads) total += Number((yield* fs.stat(file)).size)
  return total
})

/** What retiring each capture would take, read-only. */
export const plan = Effect.fn("Retire.plan")(function*(request: PlanRequest) {
  const fs = yield* FileSystem.FileSystem
  const removal = yield* Removal.Service
  const existing = (dir: string) =>
    fs.exists(dir).pipe(Effect.map((there) => (there ? dir : null)), Effect.orElseSucceed(() => null))
  const rows = yield* removal.retirePlan(request.captures)
  return yield* Effect.forEach(rows, (one) =>
    Effect.gen(function*() {
      const sessionDir = yield* existing(path.join(request.paths.sessions, stemOf(one.capture)))
      const bundles: Array<string> = []
      for (const run of one.runs) if ((yield* existing(run.bundlePath)) !== null) bundles.push(run.bundlePath)
      const artifactDir = yield* existing(Record.captureDir(request.paths.corpus, one.capture))
      const partial = {
        ...one,
        bundles,
        sessionDir,
        artifactDir,
        uploads: yield* uploadsOf(request.paths.corpus, one.capture)
      }
      const known = Object.values(one.rows).some((count) => count > 0) || sessionDir !== null || artifactDir !== null
      return { ...partial, known, bytes: request.measure ? yield* bytesOf(partial) : null } satisfies Plan as Plan
    }))
})

export interface ApplyRequest extends PlanRequest {
  readonly graceHours: number
}

export interface Applied {
  readonly plans: ReadonlyArray<Plan>
  readonly cache: ContentGc.Collected
}

/**
 * The rows of every capture in one transaction, then the files, then the
 * cache pass. Fails `Removal.Held` — nothing removed — where a running job
 * names one of the captures.
 */
export const apply = Effect.fn("Retire.apply")(function*(request: ApplyRequest) {
  const fs = yield* FileSystem.FileSystem
  const removal = yield* Removal.Service
  // The plan is read before the rows go: the ingest record that names the
  // upload lives in the artifact directory this removes.
  const plans = yield* plan(request)
  yield* removal.retire(request.captures)
  const gone = (one: string) => fs.remove(one, { recursive: true, force: true })
  for (const one of plans) {
    for (const bundle of one.bundles) yield* gone(bundle)
    if (one.sessionDir !== null) yield* gone(one.sessionDir)
    if (one.artifactDir !== null) yield* gone(one.artifactDir)
    for (const file of one.uploads) yield* fs.remove(file, { force: true })
  }
  const cache = yield* Cleanup.collectContent({
    sessions: request.paths.sessions,
    graceHours: request.graceHours,
    dryRun: false
  })
  return { plans, cache } satisfies Applied
})

const mb = (bytes: number): string => `${(bytes / 1024 / 1024).toFixed(1)} MB`

/** What one capture's retire is worth saying: a line per thing that goes. */
export const lines = (one: Plan): ReadonlyArray<string> => {
  if (!one.known) return [`${one.capture}: nothing of it in the store or on disk`]
  const rows = Removal.RETIRE_TABLES.filter((table) => one.rows[table] > 0)
    .map((table) => `${table} ${one.rows[table]}`)
  const out: Array<string> = [
    `${one.capture}:`,
    `  rows: ${rows.length === 0 ? "none" : rows.join(", ")}`,
    `  runs: ${one.runs.length === 0 ? "none" : one.runs.map((run) => `run${run.id} (${run.lane})`).join(", ")}` +
    ` — ${one.bundles.length} bundle(s) on disk`
  ]
  if (one.sessionDir !== null) out.push(`  session directory: ${one.sessionDir}`)
  if (one.artifactDir !== null) out.push(`  artifact directory: ${one.artifactDir}`)
  for (const file of one.uploads) out.push(`  upload: ${file}`)
  if (one.bytes !== null) out.push(`  ${mb(one.bytes)} of file bytes — not the disk, which moves more`)
  for (const holder of one.holders) out.push(`  HELD by job ${holder.jobId} (${holder.kind}, ${holder.how})`)
  return out
}
