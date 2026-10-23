/**
 * FORGETTING captures: the store's rows (`@sip/store` `Forget`) and the files
 * only those captures own, by explicit decision. The files are listed first,
 * the rows dropped in one transaction, the files removed after the commit,
 * then the cache pass collects the `_content/` entries no reading names.
 *
 * A capture is selected by its exact name or by a pattern over every name the
 * store keys a row by and every directory of the artifact store. Its files:
 * the bundles of its runs; its session directory `<sessions>/<stem>`, or only
 * `<sessions>/<stem>/<capture>` where a capture that stays shares the stem;
 * its artifact directory; the pcaps under `_uploads` its ingest record names;
 * the seed the deployment keeps for it (`Extraction.keptSeed`); and the job
 * records and cut logs of the jobs and preps the store drops. A name with a
 * path separator, or starting with `_` or `.`, owns no file: those entries
 * are the stores' own.
 *
 * The removals run in the order that keeps a crash between them recoverable:
 * first what only a dropped row or the artifact directory points at — the
 * job records, the cut logs, the bundles outside a session directory, the
 * uploads, the seeds — then the session directories, the artifact
 * directories last. What a crash leaves
 * is then a capture's own directories, which a second forget naming the
 * capture, or a pattern matching it, finds on disk and removes.
 */
import { Citations, Forget, Resolver } from "@sip/store"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as path from "node:path"
import type * as ContentGc from "../content/content-gc.js"
import * as Extraction from "../deployment/extraction.js"
import { unshared, uploadsHeld, uploadsOf } from "../ingest/retire.js"
import { jobDir } from "../jobs/job.js"
import { type Paths, prepDir, stemOf } from "../paths.js"
import * as Cleanup from "./cleanup.js"
import { sizeOf } from "./retention.js"

export interface Selection {
  /** Capture names, exactly as the stores key them. */
  readonly names: ReadonlyArray<string>
  /** Patterns over every name the store or the artifact store holds. */
  readonly patterns: ReadonlyArray<RegExp>
}

export interface Request extends Selection {
  readonly paths: Paths
  /** Citations to report beside those the store records. */
  readonly citations: ReadonlyArray<Citations.Citation>
  /** Whether captures still in the population may be forgotten too. */
  readonly population: boolean
}

/** Paths of one kind, absolute, and their file bytes. */
export interface FileSet {
  readonly paths: ReadonlyArray<string>
  readonly bytes: number
}

export interface Files {
  readonly sessionDirs: FileSet
  /** The bundles outside every session directory listed. */
  readonly bundles: FileSet
  readonly artifactDirs: FileSet
  readonly uploads: FileSet
  readonly seeds: FileSet
  /** The records `<sessions>/_jobs/<id>` of the jobs dropped. */
  readonly jobLogs: FileSet
  /** The cut logs `<sessions>/_preps/prep<id>` of the preps dropped. */
  readonly prepLogs: FileSet
}

/** A cited case id that names a call of a forgotten capture. */
export interface CitedCase {
  readonly ref: string
  readonly citedBy: string
  readonly capture: string
  /** Whether it stays ambiguous between calls of captures kept: it loses candidates, not its call. */
  readonly narrowed: boolean
}

export interface Plan {
  readonly store: Forget.Plan
  /** How many captures each pattern matched, in the order given. */
  readonly matched: ReadonlyArray<{ readonly pattern: string; readonly captures: number }>
  /** Names given that neither store nor disk holds anything of. */
  readonly unknown: ReadonlyArray<string>
  readonly files: Files
  /** The bundles of the dropped runs still on disk, inside a session directory or not. */
  readonly bundlesOnDisk: number
  readonly citedCases: ReadonlyArray<CitedCase>
  /** Why the forget may not run (`Forget.refusals`). */
  readonly refusals: ReadonlyArray<string>
}

/** A name that may own files: one path segment, not one of the stores' own entries. */
const owner = (name: string): boolean =>
  name !== "" && path.basename(name) === name && !name.startsWith("_") && !name.startsWith(".")

/** Whether one of `dirs` holds `file`, at any depth. */
const insideOne = (file: string, dirs: ReadonlySet<string>): boolean => {
  for (let dir = path.dirname(file); dir !== path.dirname(dir); dir = path.dirname(dir)) {
    if (dirs.has(dir)) return true
  }
  return false
}

/** The artifact store's capture directories, by name; none where the store does not exist. */
const artifactNames = (corpus: string) =>
  Effect.flatMap(FileSystem.FileSystem, (fs) => fs.readDirectory(corpus)).pipe(
    Effect.catch((error) =>
      error.reason._tag === "NotFound" ? Effect.succeed<ReadonlyArray<string>>([]) : Effect.fail(error)
    ),
    Effect.map((entries) => entries.filter(owner))
  )

const bytesOf = (one: string) =>
  Effect.gen(function*() {
    const fs = yield* FileSystem.FileSystem
    const info = yield* fs.stat(one).pipe(Effect.orElseSucceed(() => undefined))
    if (info === undefined) return 0
    return info.type === "Directory" ? yield* sizeOf(one) : Number(info.size)
  })

const setOf = (paths: ReadonlyArray<string>) =>
  Effect.gen(function*() {
    const unique = [...new Set(paths)].sort()
    let bytes = 0
    for (const one of unique) bytes += yield* bytesOf(one)
    return { paths: unique, bytes } satisfies FileSet
  })

/** The seed the deployment keeps for `capture`; none where the deployment keeps no seeds. */
const seedOf = (artifacts: string, capture: string) =>
  Effect.flatMap(Extraction.Service, (extraction) => extraction.keptSeed(artifacts, capture)).pipe(
    Effect.catchTag("Corpus.ExtractionUnstated", () => Effect.succeed(undefined))
  )

/** The candidates an ambiguous cited id is read with. */
const CANDIDATES = 1000

/** Every citation of a case id that names a call of `captures`, recorded or given. */
const citedCasesOf = Effect.fn("Forget.citedCases")(function*(
  given: ReadonlyArray<Citations.Citation>,
  captures: ReadonlySet<string>
) {
  const resolver = yield* Resolver.Service
  const recorded = yield* Effect.flatMap(Citations.Service, (citations) => citations.all)
  const seen = new Set<string>()
  const out: Array<CitedCase> = []
  for (const citation of [...recorded, ...given]) {
    const key = `${citation.ref}\u0000${citation.citedBy}`
    if (citation.kind !== "case" || seen.has(key)) continue
    seen.add(key)
    const answer = yield* resolver.resolve(
      citation.ref,
      citation.citedOn === null ? { limit: CANDIDATES } : { citedOn: citation.citedOn, limit: CANDIDATES }
    )
    const hit = answer.calls.find((call) => captures.has(call.capture))
    if (hit === undefined) continue
    const narrowed = answer.total > answer.calls.length || answer.calls.some((call) => !captures.has(call.capture))
    out.push({ ref: citation.ref, citedBy: citation.citedBy, capture: hit.capture, narrowed })
  }
  return out.sort((a, b) => (a.ref === b.ref ? (a.citedBy < b.citedBy ? -1 : 1) : a.ref < b.ref ? -1 : 1))
})

/** What forgetting the selection would take, read-only. */
export const plan = Effect.fn("Forget.plan")(function*(request: Request) {
  const fs = yield* FileSystem.FileSystem
  const store = yield* Forget.Service
  const exists = (one: string) => fs.exists(one).pipe(Effect.orElseSucceed(() => false))
  const { corpus, sessions } = request.paths

  const held = yield* store.names
  const dirs = yield* artifactNames(corpus)
  const pool = [...new Set([...held, ...dirs])].sort()
  const matched = request.patterns.map((pattern) => ({
    pattern: pattern.source,
    names: pool.filter((name) => pattern.test(name))
  }))
  const selected = [...new Set([...request.names, ...matched.flatMap((one) => one.names)])].sort()

  // A session directory shared with a capture that stays keeps its other contents.
  const chosen = new Set(selected)
  const stays = new Set(pool.filter((name) => !chosen.has(name)).map(stemOf))
  const sessionOf = (capture: string): string | null => {
    const stem = stemOf(capture)
    if (!owner(capture) || !owner(stem)) return null
    return stays.has(stem) ? path.join(sessions, stem, capture) : path.join(sessions, stem)
  }

  const inStore = new Set(held)
  const known: Array<string> = []
  const unknown: Array<string> = []
  const sessionDirs: Array<string> = []
  const artifactDirs: Array<string> = []
  const uploads: Array<string> = []
  const seeds: Array<string> = []
  for (const capture of selected) {
    let there = inStore.has(capture)
    if (owner(capture)) {
      const session = sessionOf(capture)
      if (session !== null && (yield* exists(session))) {
        sessionDirs.push(session)
        there = true
      }
      const artifactDir = path.join(corpus, capture)
      if (yield* exists(artifactDir)) {
        artifactDirs.push(artifactDir)
        uploads.push(...(yield* uploadsOf(corpus, capture)))
        there = true
      }
      const seed = yield* seedOf(corpus, capture)
      if (seed !== undefined && (yield* exists(seed))) {
        seeds.push(seed)
        there = true
      }
    }
    if (there) known.push(capture)
    else unknown.push(capture)
  }
  const gone = new Set(known)
  const owned = uploads.length === 0 ? [] : yield* unshared(uploads, yield* uploadsHeld(corpus, gone))
  const forgotten = yield* store.plan({ captures: known, citations: request.citations })
  const jobLogs: Array<string> = []
  for (const id of forgotten.invocations.jobs) {
    if (yield* exists(jobDir(sessions, id))) jobLogs.push(jobDir(sessions, id))
  }
  const prepLogs: Array<string> = []
  for (const id of forgotten.invocations.preps) {
    if (yield* exists(prepDir(sessions, id))) prepLogs.push(prepDir(sessions, id))
  }
  const removed = new Set(sessionDirs)
  const bundles: Array<string> = []
  let bundlesOnDisk = 0
  for (const run of forgotten.runs) {
    if (!(yield* exists(run.bundlePath))) continue
    bundlesOnDisk += 1
    if (!insideOne(run.bundlePath, removed)) bundles.push(run.bundlePath)
  }
  return {
    store: forgotten,
    matched: matched.map((one) => ({ pattern: one.pattern, captures: one.names.length })),
    unknown,
    files: {
      sessionDirs: yield* setOf(sessionDirs),
      bundles: yield* setOf(bundles),
      artifactDirs: yield* setOf(artifactDirs),
      uploads: yield* setOf(owned),
      seeds: yield* setOf(seeds),
      jobLogs: yield* setOf(jobLogs),
      prepLogs: yield* setOf(prepLogs)
    },
    bundlesOnDisk,
    citedCases: yield* citedCasesOf(request.citations, gone),
    refusals: Forget.refusals(forgotten, request.population)
  } satisfies Plan
})

export interface ApplyRequest extends Request {
  readonly graceHours: number
}

export interface Applied {
  readonly plan: Plan
  readonly cache: ContentGc.Collected
}

/**
 * The plan, its rows in one transaction, its files, then the cache pass.
 * Fails `Forget.Refused`, removing nothing, where the plan names a refusal.
 */
export const apply = Effect.fn("Forget.apply")(function*(request: ApplyRequest) {
  const fs = yield* FileSystem.FileSystem
  const store = yield* Forget.Service
  // The files are listed before any row goes: the ingest record naming an
  // upload lives in the artifact directory this removes.
  const planned = yield* plan(request)
  if (planned.refusals.length > 0) return yield* new Forget.Refused({ reasons: planned.refusals })
  const done = yield* store.execute({
    captures: planned.store.captures,
    citations: request.citations,
    population: request.population
  })
  const { files } = planned
  // What only a dropped row or the artifact directory points at goes first,
  // the artifact directory last: a crash leaves what a second forget finds.
  for (const one of [...files.jobLogs.paths, ...files.prepLogs.paths, ...files.bundles.paths]) {
    yield* fs.remove(one, { recursive: true, force: true })
  }
  for (const one of [...files.uploads.paths, ...files.seeds.paths]) yield* fs.remove(one, { force: true })
  for (const one of [...files.sessionDirs.paths, ...files.artifactDirs.paths]) {
    yield* fs.remove(one, { recursive: true, force: true })
  }
  const cache = yield* Cleanup.collectContent({
    sessions: request.paths.sessions,
    graceHours: request.graceHours,
    dryRun: false
  })
  return { plan: { ...planned, store: done }, cache } satisfies Applied
})

const mb = (bytes: number): string => `${(bytes / 1024 / 1024).toFixed(1)} MB`

/** What a forget is worth saying: the selection, the rows per table, the files, the pins, the cited ids. */
export const lines = (one: Plan): ReadonlyArray<string> => {
  const { store, files } = one
  const sets = [
    ["session director(ies)", files.sessionDirs],
    ["bundle(s) outside them", files.bundles],
    ["artifact director(ies)", files.artifactDirs],
    ["upload(s)", files.uploads],
    ["kept seed(s)", files.seeds],
    ["job record(s)", files.jobLogs],
    ["cut log(s)", files.prepLogs]
  ] as const
  const total = sets.reduce((sum, [, set]) => sum + set.bytes, 0)
  const lost = one.citedCases.filter((cited) => !cited.narrowed)
  const narrowed = one.citedCases.length - lost.length
  return [
    `[forget] ${store.captures.length} capture(s)` +
    (one.matched.length === 0 ? "" : `; ${one.matched.map((m) => `/${m.pattern}/ matched ${m.captures}`).join(", ")}`),
    ...one.unknown.map((name) => `[forget] ${name}: nothing of it in the store or on disk`),
    ...(store.population.length === 0 ? [] : [`[forget] in the population: ${store.population.join(", ")}`]),
    "[forget] rows:",
    ...Forget.TABLES.map((table) => `[forget]   ${table.padEnd(18)} ${store.rows[table]}`),
    `[forget] mixed, kept: ${store.mixed.jobs} job(s), ${store.mixed.requests} request(s), ` +
    `${store.mixed.sweeps} sweep(s) naming a capture that stays too`,
    "[forget] files:",
    ...sets.map(([label, set]) => `[forget]   ${String(set.paths.length).padStart(6)} ${label.padEnd(24)} ${mb(set.bytes)}`),
    `[forget]   ${one.bundlesOnDisk} bundle(s) on disk of ${store.runs.length} run(s); ${mb(total)} of file bytes in all`,
    `[forget] pinned runs: ${store.pins.length}`,
    ...store.pins.map((pin) => `[forget]   run${pin.runId}${pin.ticket === null ? "" : ` pinned by ${pin.ticket}`}`),
    `[forget] cited ids: ${store.cited.length + lost.length} it takes, ${narrowed} ambiguous case id(s) it ` +
    "takes candidates of",
    ...store.cited.map((cited) => `[forget]   ${cited.kind} ${cited.ref} cited by ${cited.citedBy}`),
    ...lost.map((cited) => `[forget]   case ${cited.ref} cited by ${cited.citedBy} (${cited.capture})`),
    ...one.refusals.map((reason) => `[forget] refused: ${reason}`)
  ]
}
