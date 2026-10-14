/**
 * `job` — jobs in the store, from the command line:
 *
 *   job start replay --request <id> [--prep N] [--batch N] [--concurrency N]
 *                    [--memory-max SIZE] [--starts-per-second N] [--no-cleanup] [--wait S]
 *   job start ingest <path>... [--set <file>]... [--cut]        (= source add)
 *   job start cut [--stale-only | --force] <capture>... | --standing | --all | --known [--slice N]
 *   job start retire <capture>... [--measure]                (= retire --yes, detached)
 *   job start delete <run id>... | --request <id> | --sweep <id> | --of-job <id> | --lane L --older-than H [--measure]
 *   job list [--limit N]
 *   job show <id>
 *   job wait <id> [--timeout S]
 *   job stop <id> [--grace S]
 *   job restart <id> --resume | --again
 *   job reap [--dry-run]
 *
 * `start` makes the row and launches the command DETACHED (`@sip/corpus`
 * `job.ts`), then returns: the job runs whatever happens to this shell. An
 * `ingest` job is the source command's `add` over paths on the host, a `cut`
 * job is the cut command over a capture list, `stale-only` unless forced, a
 * `retire` job is `retire --yes` over captures and a `delete` job `delete
 * --yes` over a run selector — the four through `job-start.ts`, each having
 * said what goes when asked by hand. It starts at once — what serialises jobs
 * is the locks they take inside, and while one waits its row says whose.
 * `show` reads the row and its progress in one statement; `list` is the
 * queue, newest first. `wait` blocks on the row. `reap` applies the liveness
 * rule by hand: a `running` job whose process is gone is closed as its record
 * says, its open runs with it.
 *
 * `stop` writes the stop and its grace on the row, then TERMs the job's
 * process group: the replay admits no new cell, waits the grace for the cells
 * in flight, closes its rows `interrupted` and exits 130. `restart` is a NEW
 * job over the same request with the old job's knobs: `--resume` replays the
 * request's cells no job of it ran, every capture as a `cases` set;
 * `--again` replays every cell as the request resolves. Both re-resolve
 * against the standing prep when they run.
 */
import { selectorOf } from "@sip/corpus/housekeeping/delete-runs"
import * as Reap from "@sip/corpus/housekeeping/reap"
import * as Discover from "@sip/corpus/ingest/discover"
import * as Source from "@sip/corpus/ingest/source"
import * as Job from "@sip/corpus/jobs/job"
import { DEFAULT_GRACE } from "@sip/corpus/jobs/job"
import { listLine, showLines } from "@sip/corpus/jobs/job-lines"
import * as JobStart from "@sip/corpus/jobs/job-start"
import {
  DEFAULT_BATCH,
  DEFAULT_CLEANUP,
  DEFAULT_CONCURRENCY,
  DEFAULT_MEMORY_MAX,
  DEFAULT_STARTS_PER_SECOND
} from "@sip/corpus/knobs"
import type { Paths } from "@sip/corpus/paths"
import { Jobs, Requests } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import { Argument, Command, Flag } from "effect/unstable/cli"
import * as path from "node:path"
import type { CliFacts } from "../facts.js"
import { pathFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { overridesOf, pathsOf } from "../options.js"
import * as Refusals from "../refusals.js"
import { exit, operatorCwd } from "../runner.js"

const usage = (facts: CliFacts): string =>
  `usage: ${facts.invocation("job")} start replay --request <id> [knobs…] | ` +
  "start ingest <path>… [--set <file>]… [--cut] | " +
  "start cut [--stale-only|--force] <capture>…|--standing|--all|--known [--slice N] | start retire <capture>… | " +
  "start delete <run>…|--request <id>|--sweep <id>|--of-job <id>|--lane L --older-than H | list [--limit N] | " +
  "show <id> | wait <id> [--timeout S] | stop <id> [--grace S] | restart <id> --resume|--again | reap [--dry-run]"

const POLL_MS = 2_000

const startFlags = {
  request: IdFlags.flag("request", "request").pipe(Flag.withDescription("The request to replay"), Flag.optional),
  prep: IdFlags.flag("prep", "prep").pipe(Flag.withDescription("The prep every capture is read as of"), Flag.optional),
  batch: Flag.integer("batch").pipe(Flag.withDescription("Cells per batch"), Flag.withDefault(DEFAULT_BATCH)),
  concurrency: Flag.integer("concurrency").pipe(
    Flag.withDescription("Cells at once"),
    Flag.withDefault(DEFAULT_CONCURRENCY)
  ),
  memoryMax: Flag.string("memory-max").pipe(
    Flag.withDescription("The memory scope the driver runs under"),
    Flag.withDefault(DEFAULT_MEMORY_MAX)
  ),
  startsPerSecond: Flag.float("starts-per-second").pipe(
    Flag.withDescription("Cell starts per second (0: as fast as slots free)"),
    Flag.withDefault(DEFAULT_STARTS_PER_SECOND)
  ),
  cleanup: Flag.boolean("cleanup").pipe(
    Flag.withDescription("Run the retention pass after the replay; --no-cleanup keeps the tree as it left it"),
    Flag.withDefault(DEFAULT_CLEANUP)
  ),
  wait: Flag.float("wait").pipe(Flag.withDescription("Seconds to wait for a held lane or box"), Flag.optional),
  // An ingest's paths or a cut's captures, and the words of each.
  items: Argument.string("item").pipe(
    Argument.withDescription("ingest: a pcap or a directory of them; cut: a capture name"),
    Argument.variadic()
  ),
  set: Flag.string("set").pipe(
    Flag.withDescription("ingest: a file of ONE capture split across consecutive files; repeatable"),
    Flag.atLeast(0)
  ),
  cut: Flag.boolean("cut").pipe(Flag.withDescription("ingest: cut what is stale once it has landed"), Flag.withDefault(false)),
  staleOnly: Flag.boolean("stale-only").pipe(
    Flag.withDescription("cut: only the captures whose standing cut is not current (the default)"),
    Flag.withDefault(false)
  ),
  force: Flag.boolean("force").pipe(Flag.withDescription("cut: every named capture, current or not"), Flag.withDefault(false)),
  standing: Flag.boolean("standing").pipe(Flag.withDescription("cut: the population"), Flag.withDefault(false)),
  all: Flag.boolean("all").pipe(Flag.withDescription("cut: the whole universe"), Flag.withDefault(false)),
  known: Flag.boolean("known").pipe(Flag.withDescription("cut: the queue"), Flag.withDefault(false)),
  slice: Flag.integer("slice").pipe(Flag.withDescription("cut: captures per slice"), Flag.optional),
  measure: Flag.boolean("measure").pipe(
    Flag.withDescription("retire, delete: recurse what goes to report the bytes"),
    Flag.withDefault(false)
  ),
  sweep: IdFlags.flag("sweep", "sweep").pipe(Flag.withDescription("delete: every run of this sweep"), Flag.optional),
  ofJob: IdFlags.flag("of-job", "job").pipe(Flag.withDescription("delete: every run this job opened"), Flag.optional),
  lane: Flag.string("lane").pipe(Flag.withDescription("delete: with --older-than, the lane"), Flag.optional),
  olderThan: Flag.float("older-than").pipe(
    Flag.withDescription("delete: with --lane, every run finished more than this many hours ago"),
    Flag.optional
  )
}

type StartInput = Command.Command.Config.Infer<typeof startFlags>

const knobsOf = (input: StartInput): JobStart.ReplayKnobs => ({
  prep: Option.getOrNull(input.prep),
  batch: input.batch,
  concurrency: input.concurrency,
  memoryMax: input.memoryMax,
  startsPerSecond: input.startsPerSecond,
  cleanup: input.cleanup,
  wait: Option.getOrNull(input.wait),
  unran: false,
  after: null
})

/** A replay launched, said: the job, its log, the commands that follow it. */
const launchReplay = Effect.fn("JobCli.launchReplay")(function*(
  facts: CliFacts,
  sessions: string,
  request: Requests.Row,
  knobs: JobStart.ReplayKnobs,
  restartOf: number | undefined
) {
  const started = yield* JobStart.replay(sessions, request, knobs, restartOf).pipe(
    Effect.catchIf(JobStart.isNotStarted, (refused) => Effect.succeed(refused))
  )
  if (started instanceof JobStart.NotStarted) {
    yield* Console.error(`[job] refused: ${started.message}`)
    return yield* exit(2)
  }
  const what = `request ${request.id} on ${request.lane}` +
    (restartOf === undefined ? "" : ` (${knobs.unran ? "resume" : "again"} of job ${restartOf})`)
  for (const line of JobStart.startedLines("replay", what, started, facts.invocation("job"))) yield* Console.log(line)
})

/** The root: the path flags every subcommand reads, and a usage line where none was named. */
const rootOf = (facts: CliFacts) =>
  Command.make("job", {}, () =>
    Effect.gen(function*() {
      yield* Console.error(usage(facts))
      yield* exit(2)
    })).pipe(
      Command.withDescription("Jobs in the store: start detached, list, show, wait, reap"),
      Command.withSharedFlags(pathFlags)
    )

type Root = ReturnType<typeof rootOf>

/** The roots the root command was given, read from inside a subcommand. */
const pathsFrom = (root: Root) => Effect.flatMap(root, (shared) => pathsOf(overridesOf(shared)))

/** The ingest job: the paths under the roots, discovered, then launched as the source command's `add`. */
const startIngest = Effect.fn("JobCli.startIngest")(function*(facts: CliFacts, input: StartInput, paths: Paths) {
  const cwd = operatorCwd()
  if (input.items.length === 0 && input.set.length === 0) {
    yield* Console.error("[job] refused: an ingest job names the paths to add")
    return yield* exit(2)
  }
  const outside = yield* Source.refusal([...input.items, ...input.set], cwd, paths)
  if (outside !== null) {
    yield* Console.error(`[job] refused: ${outside}`)
    return yield* exit(2)
  }
  const found = Discover.join(yield* Discover.discover(input.items, cwd), yield* Discover.set(input.set, cwd))
  if (found.missing.length > 0) {
    yield* Console.error(`[job] refused: ${found.missing.join("; ")}`)
    return yield* exit(2)
  }
  if (found.sources.length === 0) {
    yield* Console.error("[job] refused: the paths named no pcap")
    return yield* exit(2)
  }
  const refused = input.cut ? yield* Refusals.cut(facts, paths) : null
  if (refused !== null) {
    yield* Console.error(`[job] refused: ${refused}`)
    return yield* exit(2)
  }
  for (const said of found.collisions) yield* Console.error(`   ! ${said}`)
  const started = yield* JobStart.ingest(paths.sessions, {
    paths: input.items.map((one) => path.resolve(cwd, one)),
    set: input.set.map((one) => path.resolve(cwd, one)),
    captures: found.sources.map((source) => source.capture),
    cut: input.cut,
    concurrency: input.concurrency,
    memoryMax: input.memoryMax
  }).pipe(Effect.catchIf(JobStart.isNotStarted, (refused) => Effect.succeed(refused)))
  if (started instanceof JobStart.NotStarted) {
    yield* Console.error(`[job] refused: ${started.message}`)
    return yield* exit(2)
  }
  const what = `${found.sources.length} source(s)${input.cut ? " + cut" : ""}`
  for (const line of JobStart.startedLines("ingest", what, started, facts.invocation("job"))) yield* Console.log(line)
})

/** The cut job: a capture list and a mode, launched as the cut command's `--job`. */
const startCut = Effect.fn("JobCli.startCut")(function*(facts: CliFacts, input: StartInput, paths: Paths) {
  if (input.staleOnly && input.force) {
    yield* Console.error("[job] refused: --stale-only and --force say opposite things")
    return yield* exit(2)
  }
  if (input.items.length === 0 && !input.standing && !input.all && !input.known) {
    yield* Console.error("[job] refused: a cut job names its captures, or --standing, --all or --known")
    return yield* exit(2)
  }
  const refused = yield* Refusals.cut(facts, paths)
  if (refused !== null) {
    yield* Console.error(`[job] refused: ${refused}`)
    return yield* exit(2)
  }
  const request: JobStart.CutRequest = {
    captures: input.items,
    standing: input.standing,
    all: input.all,
    known: input.known,
    mode: input.force ? "force" : "stale-only",
    slice: Option.getOrUndefined(input.slice),
    concurrency: input.concurrency,
    memoryMax: input.memoryMax
  }
  const started = yield* JobStart.cut(paths.sessions, request).pipe(
    Effect.catchIf(JobStart.isNotStarted, (refused) => Effect.succeed(refused))
  )
  if (started instanceof JobStart.NotStarted) {
    yield* Console.error(`[job] refused: ${started.message}`)
    return yield* exit(2)
  }
  const what = input.standing
    ? "the population"
    : input.all
    ? "the universe"
    : input.known
    ? "the queue"
    : `${input.items.length} capture(s)`
  for (const line of JobStart.startedLines("cut", `${what} (${request.mode})`, started, facts.invocation("job"))) yield* Console.log(line)
})

/** The retire job: the captures named, launched as `retire --yes`; the plan is read by hand first. */
const startRetire = Effect.fn("JobCli.startRetire")(function*(facts: CliFacts, input: StartInput, paths: Paths) {
  if (input.items.length === 0) {
    yield* Console.error("[job] refused: a retire job names its captures")
    return yield* exit(2)
  }
  const captures = [...new Set(input.items)]
  const started = yield* JobStart.retire(paths.sessions, { captures, measure: input.measure }).pipe(
    Effect.catchIf(JobStart.isNotStarted, (refused) => Effect.succeed(refused))
  )
  if (started instanceof JobStart.NotStarted) {
    yield* Console.error(`[job] refused: ${started.message}`)
    return yield* exit(2)
  }
  for (const line of JobStart.startedLines("retire", `${captures.length} capture(s)`, started, facts.invocation("job"))) yield* Console.log(line)
})

/** The delete job: one run selector, launched as `delete --yes`. */
const startDelete = Effect.fn("JobCli.startDelete")(function*(facts: CliFacts, input: StartInput, paths: Paths) {
  const ids: Array<number> = []
  for (const item of input.items) {
    const id = Number(item)
    if (!Number.isInteger(id) || id <= 0) {
      yield* Console.error(`[job] refused: ${JSON.stringify(item)} is not a run id`)
      return yield* exit(2)
    }
    ids.push(id)
  }
  const selector = selectorOf({
    ids,
    request: input.request,
    sweep: input.sweep,
    ofJob: input.ofJob,
    lane: input.lane,
    olderThan: input.olderThan
  })
  if (typeof selector === "string") {
    yield* Console.error(`[job] refused: ${selector}`)
    return yield* exit(2)
  }
  const started = yield* JobStart.deleteRuns(paths.sessions, { selector, measure: input.measure }).pipe(
    Effect.catchIf(JobStart.isNotStarted, (refused) => Effect.succeed(refused))
  )
  if (started instanceof JobStart.NotStarted) {
    yield* Console.error(`[job] refused: ${started.message}`)
    return yield* exit(2)
  }
  for (const line of JobStart.startedLines("delete", JobStart.selectorArgs(selector).join(" "), started, facts.invocation("job"))) {
    yield* Console.log(line)
  }
})

const startCommand = (facts: CliFacts, root: Root) =>
  Command.make(
    "start",
    { kind: Argument.string("kind").pipe(Argument.withDescription("replay | ingest | cut | retire | delete")), ...startFlags },
    (input) =>
      Effect.gen(function*() {
        const kind = input.kind
        if (!(Jobs.KINDS as ReadonlyArray<string>).includes(kind)) {
          yield* Console.error(`[job] refused: ${kind} is none of ${Jobs.KINDS.join(", ")}`)
          return yield* exit(2)
        }
        const { sessions } = yield* pathsFrom(root)
        if (kind === "ingest") return yield* startIngest(facts, input, yield* pathsFrom(root))
        if (kind === "cut") return yield* startCut(facts, input, yield* pathsFrom(root))
        if (kind === "retire") return yield* startRetire(facts, input, yield* pathsFrom(root))
        if (kind === "delete") return yield* startDelete(facts, input, yield* pathsFrom(root))
        if (Option.isNone(input.request)) {
          yield* Console.error("[job] refused: a replay job names its request (--request <id>)")
          return yield* exit(2)
        }
        const requests = yield* Requests.Service
        const jobs = yield* Jobs.Service
        const shown = yield* requests.show(input.request.value)
        if (shown === null) {
          yield* Console.error(`[job] refused: there is no request ${input.request.value}`)
          return yield* exit(2)
        }
        yield* launchReplay(facts, sessions, shown.request, knobsOf(input), undefined)
      })
  ).pipe(Command.withDescription("Create a job and launch it detached"))

const ENDED: ReadonlySet<Jobs.Status> = new Set<Jobs.Status>(["done", "failed", "interrupted", "died"])

/**
 * A stop asked of a running job: written on the row first — when,
 * and the grace its cells in flight get — then SIGTERM to its process group.
 * The row closes `interrupted` when the process has wound down; `wait` reads
 * it. A row whose process is already gone is the liveness rule's, not this.
 */
const stopCommand = (facts: CliFacts) =>
  Command.make(
    "stop",
    {
      id: IdFlags.argument("id", "job").pipe(Argument.withDescription("The job")),
      grace: Flag.float("grace").pipe(
        Flag.withDescription("Seconds the cells in flight get before their interpreters are interrupted"),
        Flag.withDefault(DEFAULT_GRACE)
      )
    },
    (input) =>
      Effect.gen(function*() {
        const jobs = yield* Jobs.Service
        const detail = yield* jobs.show(input.id)
        if (detail === null) {
          yield* Console.error(`[job] refused: no job ${input.id}`)
          return yield* exit(2)
        }
        if (detail.job.status !== "running") {
          yield* Console.error(`[job] refused: job ${input.id} is ${detail.job.status}, not running`)
          return yield* exit(2)
        }
        if ((yield* Job.liveness(detail.job))._tag === "ended") {
          yield* Console.error(`[job] refused: job ${input.id}'s process is gone — \`${facts.invocation("job")} reap\` closes it`)
          return yield* exit(2)
        }
        yield* jobs.stop(input.id, Math.max(0, Math.round(input.grace)))
        const sent = yield* Job.stop(detail.job).pipe(Effect.catch((refused) => Effect.succeed(refused)))
        if (sent instanceof Job.NotStopped) {
          yield* Console.error(`[job] refused: ${sent.message}`)
          return yield* exit(2)
        }
        yield* Console.log(
          `job ${input.id}: stop requested (pid ${detail.job.pid}, grace ${input.grace} s) — \`${facts.invocation("job")} wait ${input.id}\``
        )
      })
  ).pipe(Command.withDescription("Ask a running job to stop: no new cell, a grace for the cells in flight, rows closed interrupted"))

/**
 * A NEW job over the request of an ended one, with its knobs:
 * `--resume` the cells no job of the request ran, as cases sets;
 * `--again` every cell as the request resolves. Refused while the job runs.
 */
const restartCommand = (facts: CliFacts, root: Root) =>
  Command.make(
    "restart",
    {
      id: IdFlags.argument("id", "job").pipe(Argument.withDescription("The job to run the request of again")),
      resume: Flag.boolean("resume").pipe(
        Flag.withDescription("Only the request's cells no job of it ran, every capture as a cases set"),
        Flag.withDefault(false)
      ),
      again: Flag.boolean("again").pipe(
        Flag.withDescription("Every cell of the request, kinds as the request resolves"),
        Flag.withDefault(false)
      )
    },
    (input) =>
      Effect.gen(function*() {
        if (input.resume === input.again) {
          yield* Console.error("[job] refused: restart takes exactly one of --resume, --again")
          return yield* exit(2)
        }
        const jobs = yield* Jobs.Service
        const requests = yield* Requests.Service
        const detail = yield* jobs.show(input.id)
        if (detail === null) {
          yield* Console.error(`[job] refused: no job ${input.id}`)
          return yield* exit(2)
        }
        const old = detail.job
        if (old.kind !== "replay" || old.requestId === null) {
          yield* Console.error(`[job] refused: job ${input.id} is a ${old.kind} job with no request to run again`)
          return yield* exit(2)
        }
        if (!ENDED.has(old.status)) {
          yield* Console.error(`[job] refused: job ${input.id} is ${old.status} — stop it first (\`${facts.invocation("job")} stop ${input.id}\`)`)
          return yield* exit(2)
        }
        const shown = yield* requests.show(old.requestId)
        if (shown === null) {
          yield* Console.error(`[job] refused: request ${old.requestId} of job ${input.id} is gone`)
          return yield* exit(2)
        }
        const { sessions } = yield* pathsFrom(root)
        yield* launchReplay(facts, sessions, shown.request, { ...JobStart.replayKnobsOf(old.args), unran: input.resume }, old.id)
      })
  ).pipe(Command.withDescription("A new job over an ended job's request: --resume its unrun cells, --again every cell"))

const list = Command.make(
  "list",
  { limit: Flag.integer("limit").pipe(Flag.withDescription("Rows to show"), Flag.withDefault(20)) },
  (input) =>
    Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      for (const row of yield* jobs.list(input.limit)) yield* Console.log(listLine(row))
    })
).pipe(Command.withDescription("The jobs, newest first"))

const show = Command.make(
  "show",
  { id: IdFlags.argument("id", "job").pipe(Argument.withDescription("The job")) },
  (input) =>
    Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const detail = yield* jobs.show(input.id)
      if (detail === null) {
        yield* Console.error(`[job] refused: no job ${input.id}`)
        return yield* exit(2)
      }
      for (const line of showLines(detail)) yield* Console.log(line)
    })
).pipe(Command.withDescription("A job and its progress"))

/**
 * Blocks until the row ends: the process's own close, or the liveness rule
 * applied here when the process is gone without one. The exit code mirrors
 * the job's; 124 on timeout, with the job still running.
 */
const wait = Command.make(
  "wait",
  {
    id: IdFlags.argument("id", "job").pipe(Argument.withDescription("The job")),
    timeout: Flag.float("timeout").pipe(Flag.withDescription("Seconds before giving up (0: never)"), Flag.withDefault(0))
  },
  (input) =>
    Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      const deadline = input.timeout > 0 ? Date.now() + input.timeout * 1000 : undefined
      for (;;) {
        const detail = yield* jobs.show(input.id)
        if (detail === null) {
          yield* Console.error(`[job] refused: no job ${input.id}`)
          return yield* exit(2)
        }
        if (ENDED.has(detail.job.status)) {
          for (const line of showLines(detail)) yield* Console.log(line)
          return yield* exit(detail.job.exit ?? (detail.job.status === "done" ? 0 : 1))
        }
        if (detail.job.status === "running" && (yield* Job.liveness(detail.job))._tag === "ended") {
          for (const line of Reap.jobLines(yield* Reap.reapJobs(false), false)) yield* Console.log(line)
          continue
        }
        if (deadline !== undefined && Date.now() >= deadline) {
          yield* Console.error(`[job] job ${input.id} still ${detail.job.status} after ${input.timeout} s`)
          return yield* exit(124)
        }
        yield* Effect.sleep(POLL_MS)
      }
    })
).pipe(Command.withDescription("Block until a job ends; the exit code is the job's"))

const reap = Command.make(
  "reap",
  { dryRun: Flag.boolean("dry-run").pipe(Flag.withDescription("Rule and close nothing"), Flag.withDefault(false)) },
  (input) =>
    Effect.gen(function*() {
      const reaped = yield* Reap.reapJobs(input.dryRun)
      const lines = Reap.jobLines(reaped, input.dryRun)
      if (lines.length === 0) yield* Console.log("[job] every running job's process is alive")
      for (const line of lines) yield* Console.log(line)
    })
).pipe(Command.withDescription("Close the running jobs whose process is gone, and their open runs"))

export const command = (facts: CliFacts) => {
  const root = rootOf(facts)
  return root.pipe(Command.withSubcommands([
    startCommand(facts, root),
    list,
    show,
    wait,
    stopCommand(facts),
    restartCommand(facts, root),
    reap
  ]))
}

