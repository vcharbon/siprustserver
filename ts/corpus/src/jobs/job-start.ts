/**
 * Jobs started: a REPLAY of a request, an INGEST over paths on the box, a CUT
 * over a capture list, a RETIRE of sources, a DELETE of runs, a CAMPAIGN
 * run (its lanes, all its cases or one, the system's data rebuilt first or
 * not) and an INBOX take (the files of one capture from the deployment's
 * inbox into its pcap root). Each is a row
 * made `queued`, the deployment's entry point for its kind (`Launcher`)
 * launched detached (`job.ts`) with `--job <id>`, so the process runs as that
 * row, and the row marked `running` with what the launcher knows. A command
 * run by hand and the same command started as a job are one call; a viewer
 * starts every kind through here.
 *
 * What was asked is on the row's `args`: a replay's knobs, so a restart
 * launches the same; the absolute paths and the captures they came to for an
 * ingest; the list's words and the mode for a cut; the captures for a retire;
 * the selector for a delete; the lanes, the case and the rebuild for a
 * campaign; the files, each as it stood when taken, for an inbox take. Only a
 * replay drives one lane, so only its row has one.
 */
import { Jobs, type Removal, type Requests } from "@sip/store"
import * as Effect from "effect/Effect"
import * as Schema from "effect/Schema"
import { Identity, Launcher } from "../deployment/index.js"
import { DEFAULT_CLEANUP } from "../knobs.js"
import * as Job from "./job.js"

/**
 * The knobs a replay job is launched with: what its row's `args` keeps. A
 * knob `null` is unstated, and the replay resolves it off the request and its
 * lane (`knob-resolution.ts`).
 */
export interface ReplayKnobs {
  readonly prep: number | null
  readonly batch: number | null
  readonly concurrency: number | null
  readonly memoryMax: string | null
  readonly startsPerSecond: number | null
  readonly cleanup: boolean
  readonly wait: number | null
  /** A resume: the request's unrun cells alone, as cases sets. */
  readonly unran: boolean
  /** A job this one waits for the end of before it resolves anything: the cut queued ahead of it. */
  readonly after: number | null
  /** Drive the lane even where it lacks a capability a capture replay needs; the runs are recorded forced. */
  readonly force: boolean
  /** The collection level the replay takes (`--collection`); `null`, the level its entry point provides, else the workspace's default. */
  readonly collection: string | null
}

/** The knobs a row's `args` keeps; a knob it does not carry reads back unstated, and a restart keeps a force and a level. */
export const replayKnobsOf = (args: Readonly<Record<string, unknown>>): ReplayKnobs => ({
  prep: typeof args["prep"] === "number" ? args["prep"] : null,
  batch: typeof args["batch"] === "number" ? args["batch"] : null,
  concurrency: typeof args["concurrency"] === "number" ? args["concurrency"] : null,
  memoryMax: typeof args["memoryMax"] === "string" ? args["memoryMax"] : null,
  startsPerSecond: typeof args["startsPerSecond"] === "number" ? args["startsPerSecond"] : null,
  cleanup: typeof args["cleanup"] === "boolean" ? args["cleanup"] : DEFAULT_CLEANUP,
  wait: typeof args["wait"] === "number" ? args["wait"] : null,
  unran: false,
  after: null,
  force: args["force"] === true,
  collection: typeof args["collection"] === "string" ? args["collection"] : null
})

/** Who starts a job or a request: the deployment's environment name, else the shell's user. */
export const createdBy: Effect.Effect<string, never, Identity.Service> = Effect.map(
  Identity.Service,
  (identity) => identity.environment ?? process.env["USER"] ?? "unknown"
)

/** What an ingest job is asked: the paths as resolved, and what discovery made of them. */
export interface IngestRequest {
  /** Pcap inputs, absolute, as the line named them. */
  readonly paths: ReadonlyArray<string>
  /** A file of further inputs, one per line, absolute: the job reads it again rather than carry the list. */
  readonly pathsFrom?: string
  /** `--set` inputs, absolute: the files of ONE capture. */
  readonly set: ReadonlyArray<string>
  /** The captures the inputs came to, recorded so the row says what was added. */
  readonly captures: ReadonlyArray<string>
  /** Cut what is stale once the ingest has landed. */
  readonly cut: boolean
  readonly concurrency: number
  readonly memoryMax: string
}

/** What a cut job is asked: the list's own words, and the mode. */
export interface CutRequest {
  readonly captures: ReadonlyArray<string>
  readonly standing: boolean
  readonly all: boolean
  readonly known: boolean
  /** `stale-only` applies the staleness rule; `force` cuts every named capture. */
  readonly mode: "stale-only" | "force"
  readonly slice: number | undefined
  readonly concurrency: number
  readonly memoryMax: string
}

/** What a retire job is asked: the captures, and whether to measure what goes. */
export interface RetireRequest {
  readonly captures: ReadonlyArray<string>
  readonly measure: boolean
}

/** What a delete job is asked: which runs, and whether to measure what goes. */
export interface DeleteRequest {
  readonly selector: Removal.Selector
  readonly measure: boolean
}

export interface Started {
  readonly id: number
  readonly launched: Job.Launched
}

/** A launch that made no process: the row is closed `failed` and this says why. */
export class NotStarted extends Schema.TaggedError<NotStarted>()("JobStart.NotStarted", { detail: Schema.String }) {
  override get message(): string {
    return this.detail
  }
}

export const isNotStarted = (error: unknown): error is NotStarted => error instanceof NotStarted

/** The arguments a replay job runs its entry point with: the request, the row, and the knobs stated, no other. */
export const replayArgs = (jobId: number, requestId: number, knobs: ReplayKnobs): ReadonlyArray<string> => [
  "--request",
  String(requestId),
  "--job",
  String(jobId),
  ...(knobs.unran ? ["--unran"] : []),
  ...(knobs.after === null ? [] : ["--after", String(knobs.after)]),
  ...(knobs.prep === null ? [] : ["--prep", String(knobs.prep)]),
  ...(knobs.batch === null ? [] : ["--batch", String(knobs.batch)]),
  ...(knobs.concurrency === null ? [] : ["--concurrency", String(knobs.concurrency)]),
  ...(knobs.memoryMax === null ? [] : ["--memory-max", knobs.memoryMax]),
  ...(knobs.startsPerSecond === null ? [] : ["--starts-per-second", String(knobs.startsPerSecond)]),
  knobs.cleanup ? "--cleanup" : "--no-cleanup",
  ...(knobs.wait === null ? [] : ["--wait", String(knobs.wait)]),
  ...(knobs.force ? ["--force"] : []),
  ...(knobs.collection === null ? [] : ["--collection", knobs.collection])
]

/** The arguments an ingest job runs its entry point with: the inputs as named, a list file by its path. */
export const ingestArgs = (id: number, request: IngestRequest): ReadonlyArray<string> => [
  "--job",
  String(id),
  ...(request.cut ? ["--cut"] : []),
  "--concurrency",
  String(request.concurrency),
  "--memory-max",
  request.memoryMax,
  ...request.set.flatMap((file) => ["--set", file]),
  ...(request.pathsFrom === undefined ? [] : ["--paths-from", request.pathsFrom]),
  ...request.paths
]

const cutArgs = (id: number, request: CutRequest): ReadonlyArray<string> => [
  "--job",
  String(id),
  ...(request.mode === "stale-only" ? ["--stale-only"] : []),
  ...(request.standing ? ["--standing"] : []),
  ...(request.all ? ["--all"] : []),
  ...(request.known ? ["--known"] : []),
  ...(request.slice === undefined ? [] : ["--slice", String(request.slice)]),
  "--concurrency",
  String(request.concurrency),
  "--memory-max",
  request.memoryMax,
  ...request.captures
]

const retireArgs = (id: number, request: RetireRequest): ReadonlyArray<string> => [
  "--job",
  String(id),
  "--yes",
  ...(request.measure ? ["--measure"] : []),
  ...request.captures
]

/** The selector as a delete command takes it on its line. */
export const selectorArgs = (selector: Removal.Selector): ReadonlyArray<string> => {
  switch (selector._tag) {
    case "ids":
      return selector.ids.map(String)
    case "request":
      return ["--request", String(selector.requestId)]
    case "sweep":
      return ["--sweep", String(selector.sweepId)]
    case "job":
      return ["--of-job", String(selector.jobId)]
    case "older":
      return ["--lane", selector.lane, "--older-than", String(selector.hours)]
  }
}

const deleteArgs = (id: number, request: DeleteRequest): ReadonlyArray<string> => [
  "--job",
  String(id),
  "--yes",
  ...(request.measure ? ["--measure"] : []),
  ...selectorArgs(request.selector)
]

/**
 * What a campaign job is asked: the campaign, its lanes, one case (by its
 * test id) and one of its sub-cases or every case, whether the system's data
 * is rebuilt from scratch before the run, the case tags it takes on the
 * lanes that exclude them, and its collection level (`null`, the level its
 * entry point provides, else the workspace's default).
 */
export interface CampaignRequest {
  readonly capture: string
  readonly lanes: ReadonlyArray<string>
  readonly case: string | null
  readonly subcase: string | null
  readonly rebuild: boolean
  readonly allow: ReadonlyArray<string>
  readonly collection: string | null
}

/** One inbox file as an inbox take found it: what tells a later copy over it from the one taken. */
export interface InboxFile {
  /** Absolute. */
  readonly path: string
  readonly size: number
  readonly mtimeMs: number
}

/** What an inbox job is asked: the files of ONE capture, in the order its reader takes them. */
export interface InboxRequest {
  readonly files: ReadonlyArray<InboxFile>
  /** The name the capture takes in the pcap root, recorded so the row says what came in. */
  readonly capture: string
}

/** The entry point's arguments of an inbox job: the row, then the files. */
export const inboxArgs = (id: number, request: InboxRequest): ReadonlyArray<string> => [
  "--job",
  String(id),
  ...request.files.flatMap((file) => ["--file", file.path])
]

/** The entry point's arguments of a campaign job. */
export const campaignArgs = (id: number, request: CampaignRequest): ReadonlyArray<string> => [
  "--job",
  String(id),
  "--campaign",
  request.capture,
  "--lanes",
  request.lanes.join(","),
  ...(request.case === null ? [] : ["--case", request.case]),
  ...(request.subcase === null ? [] : ["--subcase", request.subcase]),
  ...(request.rebuild ? ["--recreate"] : []),
  ...(request.allow.length === 0 ? [] : ["--allow", request.allow.join(",")]),
  ...(request.collection === null ? [] : ["--collection", request.collection])
]

/** The row, then the process; a launch that made no process closes the row `failed`. */
const start = Effect.fn("JobStart.start")(function*(
  sessions: string,
  kind: Launcher.JobKind,
  args: Readonly<Record<string, unknown>>,
  argsOf: (id: number) => ReadonlyArray<string>,
  row: Pick<Jobs.CreateRequest, "requestId" | "lane" | "restartOf" | "planned"> = {}
) {
  const jobs = yield* Jobs.Service
  const launcher = yield* Launcher.Service
  const id = yield* jobs.create({ kind, args, createdBy: yield* createdBy, ...row })
  const launched = yield* launcher.entry(kind).pipe(
    Effect.flatMap((entry) => Job.launch(sessions, id, [...entry, ...argsOf(id)], launcher.cwd)),
    Effect.catch((refused) => Effect.succeed(refused))
  )
  if (!("pid" in launched)) {
    yield* jobs.close(id, { status: "failed", exit: null })
    return yield* new NotStarted({ detail: launched.message })
  }
  yield* jobs.started(id, launched)
  return { id, launched } satisfies Started
})

/**
 * The row a replay job is created as: the request's cells are its plan from
 * the start, so a job queued behind another or waiting on the lane reads
 * `0/N` before its cells exist; a resume plans nothing until it resolves what
 * is left. `restartOf` marks a restart.
 */
export const replayRow = (
  request: Requests.Row,
  knobs: ReplayKnobs,
  restartOf?: number
): Pick<Jobs.CreateRequest, "requestId" | "lane" | "restartOf" | "planned"> => ({
  requestId: request.id,
  lane: request.lane,
  ...(restartOf === undefined ? {} : { restartOf }),
  ...(knobs.unran ? {} : { planned: request.cells })
})

/** A replay job over a request, on the request's lane, as `replayRow` states it. */
export const replay = (sessions: string, request: Requests.Row, knobs: ReplayKnobs, restartOf?: number) =>
  start(sessions, "replay", { ...knobs }, (id) => replayArgs(id, request.id, knobs), replayRow(request, knobs, restartOf))

export const ingest = (sessions: string, request: IngestRequest) =>
  start(sessions, "ingest", { ...request }, (id) => ingestArgs(id, request))

export const cut = (sessions: string, request: CutRequest) =>
  start(sessions, "cut", { ...request }, (id) => cutArgs(id, request))

export const retire = (sessions: string, request: RetireRequest) =>
  start(sessions, "retire", { ...request }, (id) => retireArgs(id, request))

export const deleteRuns = (sessions: string, request: DeleteRequest) =>
  start(sessions, "delete", { ...request }, (id) => deleteArgs(id, request))

export const campaign = (sessions: string, request: CampaignRequest) =>
  start(sessions, "campaign", { ...request }, (id) => campaignArgs(id, request))

/** An inbox take of one capture; its row plans the capture as its one unit. */
export const inbox = (sessions: string, request: InboxRequest) =>
  start(sessions, "inbox", { ...request }, (id) => inboxArgs(id, request), { planned: 1 })

/** Why a foreground command cannot run as the row it was named. */
export class NoSuchJob extends Schema.TaggedError<NoSuchJob>()("JobStart.NoSuchJob", { detail: Schema.String }) {
  override get message(): string {
    return this.detail
  }
}

export const isNoSuchJob = (error: unknown): error is NoSuchJob => error instanceof NoSuchJob

/**
 * The row a FOREGROUND command runs as: the one named on its line (`--job`),
 * checked to exist, else one made now, `running` under this process's pid —
 * so a verb run by hand is in the queue exactly as a launched one.
 */
export const runningAs = Effect.fn("JobStart.runningAs")(function*(
  named: number | undefined,
  kind: Jobs.Kind,
  args: Readonly<Record<string, unknown>>
) {
  const jobs = yield* Jobs.Service
  if (named !== undefined) {
    if ((yield* jobs.show(named)) === null) return yield* new NoSuchJob({ detail: `there is no job ${named}` })
    return named
  }
  const id = yield* jobs.create({ kind, args, createdBy: yield* createdBy })
  yield* jobs.started(id, { pid: process.pid })
  return id
})

/**
 * The lines a start prints: the job, its log, and the three commands that
 * follow it, spelled `<jobCommand> show|wait|stop <id>` in the deployment's
 * own command for jobs.
 */
export const startedLines = (
  kind: Jobs.Kind,
  what: string,
  started: Started,
  jobCommand: string
): ReadonlyArray<string> => [
  `job ${started.id}: ${kind} of ${what} launched (pid ${started.launched.pid})`,
  `  log: ${started.launched.logPath}`,
  `  ${jobCommand} show ${started.id} | ${jobCommand} wait ${started.id} | ${jobCommand} stop ${started.id}`
]
