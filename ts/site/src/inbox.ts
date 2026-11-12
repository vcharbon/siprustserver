/**
 * The workspace's INBOX, as the site keeps it: each capture found there is
 * taken into `pcaps/` by an `inbox` job (`@sip/lab-host` `inbox-take.ts`).
 * A drop on the site is taken at once, its files as one capture; a file
 * copied into `inbox/` is picked up by a periodic scan (every
 * {@link SCAN_INTERVAL}), once two scans in a row find it unchanged, so a
 * copy still being written is left alone. A periodic scan, not a watcher: a
 * watcher fires while a copy is half written, and a scan needs no event the
 * filesystem under a container mount may not deliver.
 *
 * A capture an active inbox job holds is not taken twice; captures whose
 * copies would take one name (`x.pcap` beside `x.pcapng`) are taken one at a
 * time, the take itself claiming the name (`inbox-take.ts`); one whose last
 * take failed is not retried while its files stand as that take found them:
 * the failed job's log says why, and copying the file anew retries it.
 * Outside a workspace there is no inbox: the scan does nothing and the
 * listing is empty.
 */
import * as Job from "@sip/corpus/jobs/job"
import * as JobStart from "@sip/corpus/jobs/job-start"
import { InboxScan, InboxTake } from "@sip/lab-host"
import { Jobs } from "@sip/store"
import { Effect, Layer, Schema } from "effect"
import * as Context from "effect/Context"
import * as Duration from "effect/Duration"
import * as Schedule from "effect/Schedule"
import * as path from "node:path"
import { ViewerConfig } from "./config.ts"
import type { Host } from "./host.ts"
import { pathsOf } from "./paths.ts"
import { SitePack } from "./site-pack.ts"

/** How often the inbox is read for copied files. */
export const SCAN_INTERVAL = Duration.seconds(5)

/** How many inbox jobs of each status a scan reads to know what is held or refused. */
const LOOKBACK = 500

/** One file of an inbox capture as the listing shows it. */
export interface File {
  readonly name: string
  readonly size: number
  readonly modifiedAt: string
}

/**
 * Where one inbox capture stands: `settling` (changed since the last scan),
 * `waiting` (settled, taken by the next scan), `taking` (an inbox job runs
 * over it), `refused` (its last take failed, the files unchanged since).
 */
export type State = "settling" | "waiting" | "taking" | "refused"

export interface Entry {
  /** The file its anonymised copy takes in `pcaps/`. */
  readonly copy: string
  readonly files: ReadonlyArray<File>
  readonly state: State
  /** The job taking it, or the one that failed to; null otherwise. */
  readonly jobId: number | null
}

export interface Started {
  readonly id: number
  readonly label: string
  readonly logPath: string
}

export class Refused extends Schema.TaggedError<Refused>()("Site.InboxRefused", { detail: Schema.String }) {}

export interface Interface {
  /** Takes these files of the inbox, absolute, as one capture: an inbox job, launched now. */
  readonly take: (files: ReadonlyArray<string>) => Effect.Effect<Started, Refused>
  /** One scan: every settled capture no job holds or refused, taken. */
  readonly scan: Effect.Effect<ReadonlyArray<Started>>
  /** The inbox's captures and where each stands. */
  readonly list: Effect.Effect<ReadonlyArray<Entry>>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Inbox") {}

/** What a row's `args.files` records: the files the take was asked for. */
const filesOfRow = (row: Jobs.Row): ReadonlyArray<JobStart.InboxFile> =>
  Array.isArray(row.args["files"]) ? (row.args["files"] as ReadonlyArray<JobStart.InboxFile>) : []

const stampOf = (file: { readonly size: number; readonly mtimeMs: number }): string => `${file.size}:${file.mtimeMs}`

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const pack = yield* SitePack.Service
    const jobs = yield* Jobs.Service
    const host = yield* Effect.context<Host>()
    const inbox = config.workspace?.inbox

    // The stamps the last scan found, by file name: what tells a settled file.
    let before: ReadonlyMap<string, string> = new Map()

    const refused = (detail: string) => new Refused({ detail })

    /** The inbox jobs holding a file (active) and those refusing it at a stamp (failed), by path. */
    const ledger = Effect.fn("Inbox.ledger")(function*() {
      const active = yield* jobs.page({ status: "active", kind: "inbox", limit: LOOKBACK, offset: 0 }).pipe(Effect.orDie)
      const holding = new Map<string, number>()
      const copies = new Map<string, number>()
      for (const row of active.rows) {
        const live = yield* Job.liveness(row).pipe(Effect.provide(host))
        if (live._tag === "ended") {
          yield* jobs.close(row.id, live.close).pipe(Effect.orDie)
          continue
        }
        for (const file of filesOfRow(row)) holding.set(file.path, row.id)
        if (typeof row.args["capture"] === "string") copies.set(row.args["capture"], row.id)
      }
      const failed = yield* jobs.page({ status: "failed", kind: "inbox", limit: LOOKBACK, offset: 0 }).pipe(Effect.orDie)
      const refusing = new Map<string, { readonly id: number; readonly stamp: string }>()
      // Newest first: the latest take of a file is the one that speaks for it.
      for (const row of failed.rows) {
        for (const file of filesOfRow(row)) {
          if (!refusing.has(file.path)) refusing.set(file.path, { id: row.id, stamp: stampOf(file) })
        }
      }
      return { holding, copies, refusing }
    })

    const launch = Effect.fn("Inbox.launch")(function*(files: ReadonlyArray<JobStart.InboxFile>) {
      const triage = config.triage
      if (triage === undefined) return yield* refused(`no triage block in ${config.configPath}: there is no store to start a job in`)
      const first = files[0]
      if (first === undefined) return yield* refused("an inbox take names the files of its capture")
      const copy = InboxTake.copyName(path.basename(first.path))
      const started = yield* JobStart.inbox(pathsOf(pack, triage).sessions, { files, capture: copy }).pipe(
        Effect.provideService(Jobs.Service, jobs),
        Effect.provide(host),
        Effect.catch((error) =>
          JobStart.isNotStarted(error) ? Effect.fail(refused(error.message)) : Effect.die(error)
        )
      )
      return {
        id: started.id,
        label: `inbox take of ${files.map((file) => path.basename(file.path)).join(" + ")} → ${copy}`,
        logPath: started.launched.logPath
      } satisfies Started
    })

    const take = Effect.fn("Inbox.take")(function*(named: ReadonlyArray<string>) {
      if (inbox === undefined) return yield* refused("no workspace: there is no inbox to take from")
      const entries = yield* InboxScan.read(inbox).pipe(Effect.provide(host))
      const files: Array<JobStart.InboxFile> = []
      for (const one of named) {
        const entry = path.dirname(one) === inbox ? entries.find((found) => found.name === path.basename(one)) : undefined
        if (entry === undefined) return yield* refused(`${one} is no capture file of the inbox ${inbox}`)
        files.push({ path: one, size: entry.size, mtimeMs: entry.mtimeMs })
      }
      return yield* launch(files)
    })

    /** The inbox's captures, each with where it stands; `before` is the last scan's stamps. */
    const standing = Effect.fn("Inbox.standing")(function*(dir: string) {
      const entries = yield* InboxScan.read(dir).pipe(Effect.provide(host))
      const { holding, copies, refusing } = yield* ledger()
      return {
        entries,
        captures: InboxScan.captures(entries).map((capture) => {
          const paths = capture.files.map((file) => path.join(dir, file.name))
          const copy = InboxTake.copyName(capture.files[0]!.name)
          const held = paths.map((one) => holding.get(one)).find((id) => id !== undefined)
          // Another capture taken under the same copy name waits for that take: one name, one take at a time.
          const queued = held === undefined ? copies.get(copy) : undefined
          const failed = capture.files.map((file, at) => {
            const last = refusing.get(paths[at]!)
            return last !== undefined && last.stamp === InboxScan.stamp(file) ? last.id : undefined
          })
          const state: State = held !== undefined
            ? "taking"
            : failed.every((id) => id !== undefined)
            ? "refused"
            : InboxScan.settled(capture, before)
            ? "waiting"
            : "settling"
          return { capture, paths, copy, queued, state, jobId: held ?? failed.find((id) => id !== undefined) ?? null }
        })
      }
    })

    const scan = Effect.gen(function*() {
      if (inbox === undefined) return [] as ReadonlyArray<Started>
      const read = yield* standing(inbox)
      before = new Map(read.entries.map((entry) => [entry.name, InboxScan.stamp(entry)]))
      const started: Array<Started> = []
      const taking = new Set<string>()
      for (const one of read.captures) {
        if (one.state !== "waiting" || one.queued !== undefined || taking.has(one.copy)) continue
        taking.add(one.copy)
        const files = one.capture.files.map((file, at) => ({ path: one.paths[at]!, size: file.size, mtimeMs: file.mtimeMs }))
        const launched = yield* launch(files).pipe(
          Effect.catch((error) => Effect.logWarning(`inbox: ${error.detail}`).pipe(Effect.as(undefined)))
        )
        if (launched !== undefined) started.push(launched)
      }
      return started as ReadonlyArray<Started>
    }).pipe(Effect.withSpan("Inbox.scan"))

    const list = Effect.gen(function*() {
      if (inbox === undefined) return [] as ReadonlyArray<Entry>
      const read = yield* standing(inbox)
      return read.captures.map((one): Entry => ({
        copy: InboxTake.copyName(one.capture.files[0]!.name),
        files: one.capture.files.map((file) => ({
          name: file.name,
          size: file.size,
          modifiedAt: new Date(file.mtimeMs).toISOString()
        })),
        state: one.state,
        jobId: one.jobId
      }))
    }).pipe(Effect.withSpan("Inbox.list"))

    return Service.of({ take, scan, list })
  })
)

/**
 * The periodic scan, for as long as the server runs: a scan that fails is
 * logged and the next one runs on time. Nothing runs outside a workspace.
 */
export const scanner = Layer.effectDiscard(
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    if (config.workspace === undefined) return
    const service = yield* Service
    yield* Effect.logInfo(`inbox ${config.workspace.inbox}: scanned every ${Duration.format(SCAN_INTERVAL)}`)
    yield* service.scan.pipe(
      Effect.catchCause((cause) => Effect.logWarning("inbox scan failed", cause)),
      Effect.repeat(Schedule.spaced(SCAN_INTERVAL)),
      Effect.forkScoped
    )
  })
)

export * as Inbox from "./inbox.ts"
