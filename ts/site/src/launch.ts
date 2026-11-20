/**
 * Jobs started from the viewer, each as the job command's `start` starts it —
 * a row, the command detached under the deployment's launcher, the row `running` — so
 * the viewer holds no slot and its restart loses nothing. A REPLAY runs a
 * request; with a re-cut first it is two jobs, the cut over the request's
 * captures and the replay launched with `--after` it. A CAMPAIGN runs a
 * campaign this deployment defines on its lanes, one case or all. A RETIRE takes captures;
 * an INGEST takes paths under the source roots as the
 * source command takes them, and the same refusals apply: a path outside the
 * roots, a path naming nothing, a cut with no census run to declare from. A
 * PCAP REPLAY takes a file of the workspace's pcap root to a lane: an ingest
 * with its cut, then a replay after it (`pcap-replay.ts`).
 */
import * as CampaignLanes from "@sip/campaign-runner/lanes"
import * as Runnable from "@sip/campaign-runner/runnable"
import { allowRefusal } from "@sip/campaign-runner/selection"
import * as Discover from "@sip/corpus/ingest/discover"
import * as Job from "@sip/corpus/jobs/job"
import * as JobStart from "@sip/corpus/jobs/job-start"
import * as Reap from "@sip/corpus/housekeeping/reap"
import * as Source from "@sip/corpus/ingest/source"
import { Batches, Jobs, Requests } from "@sip/store"
import { Effect, Layer, Schema } from "effect"
import * as Context from "effect/Context"
import * as crypto from "node:crypto"
import * as fs from "node:fs"
import * as path from "node:path"
import { ViewerConfig } from "./config.ts"
import type { Host } from "./host.ts"
import { Identity } from "@sip/corpus"
import { LaneCatalog } from "@sip/rig"
import { Campaigns } from "@sip/workspace"
import { DEFAULT_KNOBS } from "./lanes.ts"
import { type Paths, pathsOf } from "./paths.ts"
import { PcapReplay } from "./pcap-replay.ts"
import { SitePack } from "./site-pack.ts"

/**
 * What an ingest of `absolute` asks the job: the files of one capture as its
 * `--set`, else the paths as a list file under the jobs directory, named by
 * its content (`source add --paths-from`) — a selection of thousands of
 * paths on the job's command line dies of E2BIG before the job runs.
 */
export const ingestRequest = Effect.fn("JobLaunch.ingestRequest")(function*(
  sessions: string,
  absolute: ReadonlyArray<string>,
  asked: { readonly set: boolean; readonly captures: ReadonlyArray<string>; readonly cut: boolean }
) {
  const common = {
    captures: asked.captures,
    cut: asked.cut,
    concurrency: DEFAULT_KNOBS.concurrency,
    memoryMax: DEFAULT_KNOBS.memoryMax
  }
  if (asked.set) {
    const ofSet: JobStart.IngestRequest = { ...common, paths: [], set: absolute }
    return ofSet
  }
  const body = `${absolute.join("\n")}\n`
  const dir = Job.jobsDir(sessions)
  const file = path.join(dir, `paths-${crypto.createHash("sha256").update(body).digest("hex").slice(0, 12)}.txt`)
  yield* Effect.sync(() => {
    fs.mkdirSync(dir, { recursive: true })
    fs.writeFileSync(file, body)
  })
  const listed: JobStart.IngestRequest = { ...common, paths: [], pathsFrom: file, set: [] }
  return listed
})

export interface ReplayKnobs {
  readonly concurrency?: number | undefined
  readonly batch?: number | undefined
  readonly memoryMax?: string | undefined
  readonly startsPerSecond?: number | undefined
  readonly cleanup?: boolean | undefined
  readonly wait?: number | undefined
  /** The collection level the replay takes; unstated, the replay's own default (the pack's capture level). */
  readonly collection?: string | undefined
}

export type StartRequest =
  | {
    readonly kind: "replay"
    readonly requestId: number
    readonly knobs?: ReplayKnobs | undefined
    /** A cut job queued ahead of the replay, over the request's captures. */
    readonly recutFirst?: "stale-only" | "force" | undefined
  }
  | { readonly kind: "retire"; readonly captures: ReadonlyArray<string> }
  | {
    readonly kind: "campaign"
    readonly capture: string
    readonly lanes: ReadonlyArray<string>
    readonly case?: string | undefined
    readonly subcase?: string | undefined
    readonly rebuild?: boolean | undefined
    /** Campaign tags the run takes on the lanes that exclude them. */
    readonly allow?: ReadonlyArray<string> | undefined
    /** The collection level the run takes; unstated, the workspace's default. */
    readonly collection?: string | undefined
  }
  | {
    readonly kind: "ingest"
    readonly paths: ReadonlyArray<string>
    readonly set?: boolean | undefined
    readonly cut?: boolean | undefined
  }
  | {
    readonly kind: "pcap-replay"
    /** A file name of the workspace's `pcaps/`. */
    readonly file: string
    readonly lane: string
    readonly force?: boolean | undefined
  }

export interface Started {
  readonly id: number
  readonly kind: Jobs.Kind
  readonly label: string
  readonly logPath: string
}

export class Refused extends Schema.TaggedError<Refused>()("Site.JobRefused", {
  detail: Schema.String
}) {}

export interface Interface {
  /** The jobs launched, in launch order: the cut before the replay it was queued ahead of. */
  readonly start: (request: StartRequest) => Effect.Effect<ReadonlyArray<Started>, Refused>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/JobLaunch") {}

const blank = (value: string | undefined): string | undefined => (value === undefined || value === "" ? undefined : value)

/** How many active jobs a campaign launch reads for one of its environment. */
const ACTIVE_LOOKBACK = 200

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const jobs = yield* Jobs.Service
    const batches = yield* Batches.Service
    const requests = yield* Requests.Service
    const pack = yield* SitePack.Service
    const laneCatalog = pack.lanes
    const prefix = pack.campaigns.lanePrefix
    const campaignLanes = CampaignLanes.lanesOf(prefix, laneCatalog)
    const host = yield* Effect.context<Host>()

    const triagePaths = (): Paths | Refused =>
      config.triage === undefined
        ? new Refused({ detail: `no triage block in ${config.configPath}: there is no store to start a job in` })
        : pathsOf(pack, config.triage)

    /** A launch that made no process is a refusal saying why. */
    const launched = <A, E>(effect: Effect.Effect<A, E, Jobs.Service | Host>): Effect.Effect<A | Refused> =>
      effect.pipe(
        Effect.catch((error) =>
          JobStart.isNotStarted(error) ? Effect.succeed(new Refused({ detail: error.message })) : Effect.die(error)
        ),
        Effect.provideService(Jobs.Service, jobs),
        Effect.provide(host)
      )

    /**
     * The collection level `level` names checked as the job's own entry point
     * checks it before it opens (the pack's `levelRefusal`): a refusal before any
     * row is made. `null` names none, the job's own default (a replay's the
     * pack's capture level, a campaign run's the workspace's), and checks nothing.
     */
    const levelChecked = (level: string | null): Effect.Effect<void, Refused> =>
      level === null ? Effect.void : pack.levelRefusal(level, process.env).pipe(
        Effect.provide(host),
        Effect.flatMap((refused) => (refused === null ? Effect.void : Effect.fail(new Refused({ detail: refused }))))
      )

    const started = (kind: Jobs.Kind, label: string, one: JobStart.Started): Started => ({
      id: one.id,
      kind,
      label,
      logPath: one.launched.logPath
    })

    const replay = Effect.fn("JobLaunch.replay")(function*(
      paths: Paths,
      request: Extract<StartRequest, { kind: "replay" }>
    ) {
      const shown = yield* requests.show(request.requestId).pipe(Effect.orDie)
      if (shown === null) return yield* new Refused({ detail: `there is no request ${request.requestId}` })
      const row = shown.request
      const stated = request.knobs ?? {}
      // A knob the page leaves unstated stays so: the replay resolves it off the request, then its lane.
      let knobs: JobStart.ReplayKnobs = {
        prep: null,
        batch: stated.batch === undefined ? null : Math.max(1, stated.batch),
        concurrency: stated.concurrency === undefined ? null : Math.max(1, stated.concurrency),
        memoryMax: blank(stated.memoryMax) ?? null,
        startsPerSecond: stated.startsPerSecond === undefined ? null : Math.max(0, stated.startsPerSecond),
        cleanup: stated.cleanup ?? row.knobs.cleanup,
        wait: stated.wait ?? null,
        unran: false,
        after: null,
        force: false,
        collection: blank(stated.collection) ?? null
      }
      const driven = LaneCatalog.find(laneCatalog, row.lane)
      const foreign = driven === undefined ? null : yield* pack.laneRefusal(driven, process.env).pipe(Effect.provide(host))
      if (foreign !== null) return yield* new Refused({ detail: foreign })
      yield* levelChecked(knobs.collection)
      const out: Array<Started> = []
      if (request.recutFirst !== undefined) {
        const refused = yield* pack.censusRefusal(paths).pipe(Effect.provide(host))
        if (refused !== null) return yield* new Refused({ detail: refused })
        const captures = [...new Set([...shown.cells.map((cell) => cell.capture), ...row.selection.whole])].sort()
        const cut = yield* launched(JobStart.cut(paths.sessions, {
          captures,
          standing: false,
          all: false,
          known: false,
          mode: request.recutFirst,
          slice: undefined,
          concurrency: DEFAULT_KNOBS.concurrency,
          memoryMax: knobs.memoryMax ?? row.knobs.memoryMax
        }))
        if (cut instanceof Refused) return yield* cut
        out.push(started("cut", `cut of ${captures.length} capture(s) (${request.recutFirst})`, cut))
        knobs = { ...knobs, after: cut.id }
      }
      const one = yield* launched(JobStart.replay(paths.sessions, row, knobs))
      if (one instanceof Refused) return yield* one
      out.push(started(
        "replay",
        `replay of request ${row.id} on ${row.lane}` + (knobs.after === null ? "" : ` after job ${knobs.after}`),
        one
      ))
      return out as ReadonlyArray<Started>
    })

    const retire = Effect.fn("JobLaunch.retire")(function*(paths: Paths, captures: ReadonlyArray<string>) {
      const named = [...new Set(captures)]
      if (named.length === 0) return yield* new Refused({ detail: "a retire job names its captures" })
      const one = yield* launched(JobStart.retire(paths.sessions, { captures: named, measure: false }))
      if (one instanceof Refused) return yield* one
      return [started("retire", `retire of ${named.join(", ")}`, one)] as ReadonlyArray<Started>
    })

    /** Paths as the source command takes them: under the roots, resolved against the repo, discovered before any row. */
    const ingest = Effect.fn("JobLaunch.ingest")(function*(
      paths: Paths,
      request: Extract<StartRequest, { kind: "ingest" }>
    ) {
      const cwd = paths.repoRoot
      const inputs = request.paths.map((one) => one.trim()).filter((one) => one !== "")
      if (inputs.length === 0) return yield* new Refused({ detail: "an ingest job names the paths to add" })
      const outside = yield* Source.refusal(inputs, cwd, paths).pipe(Effect.provide(host), Effect.orDie)
      if (outside !== null) return yield* new Refused({ detail: outside })
      const found = request.set === true
        ? Discover.join({ sources: [], collisions: [], missing: [] }, yield* Discover.set(inputs, cwd).pipe(Effect.provide(host)))
        : yield* Discover.discover(inputs, cwd).pipe(Effect.provide(host))
      if (found.missing.length > 0) return yield* new Refused({ detail: found.missing.join("; ") })
      if (found.sources.length === 0) return yield* new Refused({ detail: "the paths named no pcap" })
      if (request.cut === true) {
        const refused = yield* pack.censusRefusal(paths).pipe(Effect.provide(host))
        if (refused !== null) return yield* new Refused({ detail: refused })
      }
      const absolute = inputs.map((one) => path.resolve(cwd, one))
      const one = yield* launched(JobStart.ingest(
        paths.sessions,
        yield* ingestRequest(paths.sessions, absolute, {
          set: request.set === true,
          captures: found.sources.map((source) => source.capture),
          cut: request.cut === true
        })
      ))
      if (one instanceof Refused) return yield* one
      const label = `ingest of ${found.sources.length} source(s)${request.cut === true ? " + cut" : ""}` +
        (found.collisions.length === 0 ? "" : ` (${found.collisions.join("; ")})`)
      return [started("ingest", label, one)] as ReadonlyArray<Started>
    })

    /**
     * A run of a campaign the deployment defines, refused naming those it does
     * where it is another: its lanes by their short or campaign name, the
     * campaign's where it names none, one case and sub-case or every case, the
     * data rebuilt or not. Refused for a lane this environment does not run (the
     * pack's campaign refusal: in a workspace, one naming another lab's container) or a pick of two lane
     * groups, naming the groups that run (`@sip/campaign-runner` `runnable.ts`), as the run itself refuses
     * them, and for a collection level the deployment's wiring refuses. Refused while a campaign job of this checkout's environment is queued
     * or its process runs (one whose process is gone is closed as it ended);
     * the run itself refuses a second run of the environment.
     */
    const campaign = Effect.fn("JobLaunch.campaign")(function*(
      paths: Paths,
      request: Extract<StartRequest, { kind: "campaign" }>
    ) {
      const env = yield* Effect.sync(() => process.env)
      const defined = yield* pack.campaigns.defined(env).pipe(
        Effect.provide(host),
        Effect.mapError((refused) => new Refused({ detail: refused.reason }))
      )
      const named = defined.find((one) => one.name === request.capture)
      if (named === undefined) return yield* new Refused({ detail: Campaigns.unknownReason(request.capture, defined) })
      const asked = request.lanes.length > 0 ? request.lanes : named.lanes ?? []
      const lanes = [...new Set(asked.map((lane) => CampaignLanes.laneNamed(prefix, laneCatalog, lane)))]
      const unknown = lanes.filter((lane) => !campaignLanes.includes(lane))
      if (lanes.length === 0) return yield* new Refused({ detail: "a campaign run names its lanes" })
      if (unknown.length > 0) {
        return yield* new Refused({ detail: `no lane ${unknown.join(", ")} (lanes: ${campaignLanes.join(", ")})` })
      }
      const refusal = yield* pack.campaigns.refusal(env).pipe(Effect.provide(host))
      const picked = lanes.flatMap((lane) => {
        const found = CampaignLanes.catalogLane(prefix, laneCatalog, lane)
        return found === undefined ? [] : [found]
      })
      const unrunnable = Runnable.pickRefusal(refusal, laneCatalog, picked)
      if (unrunnable !== null) {
        return yield* new Refused({ detail: `${unrunnable}; instead: ${Runnable.remedyOf(refusal, laneCatalog)}` })
      }
      const one = blank(request.case)
      const sub = blank(request.subcase)
      if (sub !== undefined && one === undefined) return yield* new Refused({ detail: "a sub-case is run with its case" })
      const allow = request.allow ?? []
      const declared = yield* pack.campaigns.tags(env).pipe(
        Effect.provide(host),
        Effect.mapError((refused) => new Refused({ detail: refused.reason }))
      )
      const untagged = allowRefusal(declared, allow)
      if (untagged !== undefined) return yield* new Refused({ detail: untagged })
      const collection = blank(request.collection) ?? null
      yield* levelChecked(collection)
      const own = (yield* Effect.provide(Identity.Service, host)).environment
      const page = yield* jobs.page({ status: "active", limit: ACTIVE_LOOKBACK, offset: 0 }).pipe(Effect.orDie)
      for (const job of page.rows) {
        if (job.kind !== "campaign" || job.createdBy !== (own ?? job.createdBy)) continue
        const live = yield* Job.liveness(job).pipe(Effect.provide(host))
        if (live._tag === "ended") {
          yield* batches.abandon({ jobId: job.id }, Reap.BATCH_DETAIL).pipe(Effect.orDie)
          yield* jobs.close(job.id, live.close).pipe(Effect.orDie)
          continue
        }
        return yield* new Refused({ detail: `campaign job ${job.id} is ${job.status}: one campaign run at a time` })
      }
      const run = yield* launched(JobStart.campaign(paths.sessions, {
        capture: request.capture,
        lanes,
        case: one ?? null,
        subcase: sub ?? null,
        rebuild: request.rebuild === true,
        allow,
        collection
      }))
      if (run instanceof Refused) return yield* run
      const allowing = [...new Set([...named.allow, ...allow])]
      const label = `campaign ${request.capture} ${one === undefined ? "run" : `of ${one}${sub === undefined ? "" : ` [${sub}]`}`}` +
        ` on ${lanes.join(", ")}${request.rebuild === true ? ", data rebuilt" : ""}` +
        (allowing.length === 0 ? "" : `, allowing ${allowing.join(", ")}`) +
        (collection === null ? "" : `, collection ${collection}`)
      return [started("campaign", label, run)] as ReadonlyArray<Started>
    })

    const start = Effect.fn("JobLaunch.start")(function*(request: StartRequest) {
      const paths = triagePaths()
      if (paths instanceof Refused) return yield* paths
      switch (request.kind) {
        case "replay":
          return yield* replay(paths, request)
        case "retire":
          return yield* retire(paths, request.captures)
        case "ingest":
          return yield* ingest(paths, request)
        case "campaign":
          return yield* campaign(paths, request)
        case "pcap-replay":
          return yield* PcapReplay.launch(paths, config.workspace, pack, {
            file: request.file,
            lane: request.lane,
            force: request.force === true
          }).pipe(
            Effect.mapError((refused) => new Refused({ detail: refused.detail })),
            Effect.provideService(Jobs.Service, jobs),
            Effect.provideService(Requests.Service, requests),
            Effect.provide(host)
          )
      }
    })

    return Service.of({ start })
  })
)

export * as JobLaunch from "./launch.ts"
