/**
 * The run STORE, from the command line; with no verb it names every verb, one
 * line each:
 *
 *   migrate                      # apply the schema
 *   seed                         # import run bundles with no row yet
 *   rebuild                      # forget the session rows, then re-derive
 *   status [--lane L] [--limit N]  # what the queue would render
 *   sweeps [--limit N]           # the invocations, newest first
 *   jobs [--limit N]             # the jobs, newest first
 *   runs <selector>              # the runs a selector names, newest first
 *   compare <before> <after>     # what moved; a bare integer is a sweep here
 *   where <capture> [--prep N] [--cases]  # every artifact of one capture
 *   case-doc <capture> <case> | <case id> [--prep N] [--print path|flow|ledger]
 *   delta --lane L [--against L]  # green there, red here
 *   rulehits [--limit N]         # count the rule hits of runs never measured
 *   calls-backfill [--lane L]    # measure what the per-call queue lacks
 *   pin <selector> [--ticket T] | unpin <selector> | pins  # runs kept by hand
 *   close-sweep <N>              # close a sweep whose process is gone
 *   close-prep <N>               # finish a prep whose cut is gone
 *
 * A deployment adds verbs of its own beside these (`command`'s `extra`).
 *
 * The verbs are wiring: what seed and rebuild import is `@sip/corpus`
 * `BundleImport`, what the backfills measure is `Backfill`, what `where`
 * reads is `Where`.
 */
import { excludedFile, readReasons } from "@sip/corpus/cut/excluded"
import * as Backfill from "@sip/corpus/housekeeping/backfill"
import * as BundleImport from "@sip/corpus/housekeeping/bundle-import"
import * as PrepClose from "@sip/corpus/housekeeping/prep-close"
import * as SweepClose from "@sip/corpus/housekeeping/sweep-close"
import { listLine as jobLine } from "@sip/corpus/jobs/job-lines"
import type { Paths } from "@sip/corpus/paths"
import * as CaseDoc from "@sip/corpus/report/case-doc"
import * as Where from "@sip/corpus/report/where"
import { read as readOverlay } from "@sip/corpus/triage-overlay"
import { captures as universeOf } from "@sip/corpus/universe"
import { Delta, Jobs, Model, Pins, Queue, Runs, RunSets, StoreBinding, Sweeps } from "@sip/store"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as Option from "effect/Option"
import * as Path from "effect/Path"
import { Argument, Command, Flag } from "effect/unstable/cli"
import type { CliFacts } from "../facts.js"
import { laneFlag, pathFlags } from "../flags.js"
import * as IdFlags from "../id-flags.js"
import { overridesOf, pathsOf } from "../options.js"
import { exit } from "../runner.js"
import * as Compare from "./compare.js"

const limitFlag = Flag.integer("limit").pipe(
  Flag.withDescription("Rows to show"),
  Flag.withDefault(20)
)

/** All bundles no row records yet, imported, and what the import found said. */
const ingest = Effect.fn("Store.ingest")(function*(paths: Paths, lane: string) {
  const done = yield* BundleImport.importBundles(paths.sessions, lane)
  yield* Console.log(
    `[store] ${done.onDisk} bundle(s) on disk: imported=${done.imported} already-recorded=${done.alreadyRecorded}`
  )
})

/**
 * The distinct reason tokens the cut refused a capture under — the whole
 * content of a `no-cases` row, so the row prints them the way a `skipped` row
 * prints its lane blocks. Empty where the cut left no record.
 */
const refusedUnder = (path: Path.Path, paths: Paths, capture: string) =>
  readReasons(excludedFile(path, paths.sessions, capture))

const refusedLine = (reasons: ReadonlyArray<string>): string =>
  reasons.length === 0 ? "  refused: (no record)" : `  refused: ${reasons.join(", ")}`

const status = Effect.fn("Store.status")(function*(paths: Paths, lane: string, limit: number) {
  const runs = yield* Runs.Service
  const path = yield* Path.Path
  const standings = yield* runs.standings(lane)
  const { registry, tickets } = yield* readOverlay(paths.registry)
  const captures = yield* universeOf(standings.map((one) => one.capture))
  const view = Queue.overlay({ standings, registry, tickets, captures })
  yield* Console.log(
    `[store] lane=${lane} recorded=${standings.length} universe=${captures.length} ` +
      `never-run=${view.neverRun.length}`
  )
  for (const key of Queue.QUEUE_ORDER) yield* Console.log(`   ${key.padEnd(22)} ${view.totals[key] ?? 0}`)
  yield* Console.log(`   ${"off-queue records".padEnd(22)} ${view.offQueue.length}`)
  for (const row of view.rows.slice(0, limit)) {
    const refused = row.status !== "no-cases" ? "" : refusedLine(yield* refusedUnder(path, paths, row.capture))
    yield* Console.log(
      `${row.capture}  ${row.status}  run${row.runId} ${row.failed}/${row.cases.length} failed` +
        (row.rfcGating === 0 ? "" : `  rfc: ${row.rfcGating} gating`) +
        (row.skipReasons.length === 0 ? "" : `  skipped: ${row.skipReasons.join(", ")}`) +
        refused +
        (row.lastOkRunId === undefined ? "  (never green)" : `  last-ok run${row.lastOkRunId}`)
    )
  }
})

/**
 * Every artifact of ONE capture (`Where`), one line each; a capture nothing
 * names is said to be unknown, and the cases are counted unless `cases` lists them.
 */
const where = Effect.fn("Store.where")(function*(
  facts: CliFacts,
  paths: Paths,
  capture: string,
  options: { readonly prep: number | undefined; readonly cases: boolean }
) {
  const read = yield* Where.read(paths, capture, {
    prep: options.prep,
    cases: options.cases,
    cutCommand: (named) => `${facts.invocation("cut")} ${named}`
  })
  yield* Console.log(capture)
  if (!read.known) {
    yield* Console.log(
      "  ! no capture by this name in the artifact store, the cut store or a run — name it in full, " +
        "as the corpus directory is named"
    )
  }
  for (const entry of read.entries) yield* Console.log(`  ${entry.label.padEnd(10)} ${entry.text}`)
  if (!options.cases && read.cases > 0) {
    yield* Console.log(`  ${"cases".padEnd(10)} ${read.cases} case(s) — --cases lists each case directory`)
  }
})

/** The rule hits of runs never measured, counted, and what was found said. */
const backfillRuleHits = Effect.fn("Store.backfillRuleHits")(function*(limit: number) {
  const done = yield* Backfill.ruleHits(limit)
  yield* Console.log(
    `[store] rule hits: ${done.measured} run(s) measured over ${done.cells} cell(s); ` +
      `${done.gone} of ${done.pending} unmeasured run(s) have no readable bundle left`
  )
})

/** The per-call measures the queue lacks, backfilled, and what was found said. */
const backfillCalls = Effect.fn("Store.backfillCalls")(function*(paths: Paths, lane: string | undefined) {
  const done = yield* Backfill.calls(paths, lane)
  yield* Console.log(
    `[store] main errors: ${done.errors.measured} run(s) re-measured over ${done.errors.rows} case row(s); ` +
      `${done.errors.gone} of ${done.errors.pending} unmeasured run(s) have no readable bundle left`
  )
  const { firstTime, recorded, refused, standing } = done.refusals
  yield* Console.log(
    `[store] refusals: ${recorded} capture(s) recorded (${firstTime} for the first time), ` +
      `${refused} refused call(s); ${standing - recorded} of ${standing} standing cut(s) ` +
      "have no record on disk"
  )
})

/** One sweep as a line: what it swept, how, and the tree it swept on. */
const sweepLine = (row: Sweeps.Row): string =>
  `sweep ${String(row.id).padStart(4)}  ${row.startedAt}  ${row.selection ?? `--${row.universe}`}  ` +
  `${String(row.runs).padStart(5)} run(s)  x${row.concurrency}  ` +
  `${row.finishedAt === null ? "UNFINISHED" : "done"}  ` +
  `tree ${Model.codeLine(row.code, "tree")}`

/** One run as a listing line: who made it, on which code, and what it came to. */
const runLine = (row: RunSets.RunRow): string =>
  `run${row.id}  ${row.startedAt}  ${row.capture}  ${row.lane}  ${row.setKind}  ${row.status}  ` +
  `${row.failed}/${row.cases} failed  env ${row.env ?? "unknown"}@${row.branch ?? "unknown"}  ` +
  `code ${Model.codeLine(row.code, "sha")}` +
  (row.sweepId === null ? "" : `  sweep ${row.sweepId}`) +
  (row.jobId === null ? "" : `  job ${row.jobId}`) +
  (row.cleanup === null ? "" : `  DIRTY cleanup: ${row.cleanup}`)

/** The runs a pin line names: one selector, every run it admits. */
const runsNamed = Effect.fn("Store.runsNamed")(function*(selector: string) {
  const parsed = yield* StoreBinding.parse(selector)
  if ("refused" in parsed) return parsed.refused
  const sets = yield* RunSets.Service
  const refused = yield* sets.refusal(parsed)
  if (refused !== null) return refused
  return (yield* sets.runs(parsed, Number.MAX_SAFE_INTEGER)).map((row) => row.id)
})

/** The root of a store command named `name`: the path flags every verb reads. */
const rootOf = <Name extends string>(facts: CliFacts, name: Name, verbLines: () => ReadonlyArray<string>) =>
  Command.make(name, {}, () =>
    Effect.gen(function*() {
      yield* Console.error(
        [`usage: ${name} <verb> [flags]   (${name} <verb> --help for its flags)`, "", ...verbLines(), "", facts.frontDoors]
          .join("\n")
      )
      yield* exit(2)
    })).pipe(
      Command.withDescription(`The run store, from the command line\n\n${facts.frontDoors}`),
      Command.withSharedFlags(pathFlags)
    )

/**
 * The store's verbs, each reading the roots the root command was given
 * through `rootPaths`, in the order the help lists them.
 */
export const verbs = <E, R>(facts: CliFacts, rootPaths: Effect.Effect<Paths, E, R>) => {
  const migrate = Command.make("migrate", {}, () =>
    Effect.gen(function*() {
      const binding = yield* StoreBinding.Service
      const applied = yield* binding.migrate
      yield* Console.log(
        applied.length === 0
          ? "[store] schema already current"
          : `[store] applied ${applied.map(([id, name]) => `${id}_${name}`).join(", ")}`
      )
    })).pipe(Command.withDescription("Apply the schema"))

  const seed = Command.make("seed", { lane: laneFlag(facts) }, (input) =>
    Effect.gen(function*() {
      yield* ingest(yield* rootPaths, input.lane)
    })).pipe(Command.withDescription("Import run bundles with no row yet"))

  const rebuild = Command.make("rebuild", { lane: laneFlag(facts) }, (input) =>
    Effect.gen(function*() {
      const at = yield* rootPaths
      const runs = yield* Runs.Service
      const dropped = yield* runs.forget(at.sessions)
      yield* Console.log(`[store] forgot ${dropped} run row(s) under ${at.sessions}`)
      yield* ingest(at, input.lane)
    })).pipe(Command.withDescription("Forget the session rows, then re-derive"))

  const statusCommand = Command.make("status", { lane: laneFlag(facts), limit: limitFlag }, (input) =>
    Effect.gen(function*() {
      yield* status(yield* rootPaths, input.lane, input.limit)
    })).pipe(Command.withDescription("What the queue would render"))

  const sweepsCommand = Command.make("sweeps", { limit: limitFlag }, (input) =>
    Effect.gen(function*() {
      const sweeps = yield* Sweeps.Service
      for (const row of yield* sweeps.list(input.limit)) yield* Console.log(sweepLine(row))
    })).pipe(Command.withDescription("The invocations, newest first"))

  const jobsCommand = Command.make("jobs", { limit: limitFlag }, (input) =>
    Effect.gen(function*() {
      const jobs = yield* Jobs.Service
      for (const row of yield* jobs.list(input.limit)) yield* Console.log(jobLine(row))
    })).pipe(Command.withDescription("The jobs, newest first"))

  const runsCommand = Command.make(
    "runs",
    {
      selector: Argument.string("selector").pipe(Argument.withDescription(`Which runs: ${facts.selectorForms}`)),
      limit: limitFlag
    },
    (input) =>
      Effect.gen(function*() {
        const parsed = yield* StoreBinding.parse(input.selector)
        if ("refused" in parsed) {
          yield* Console.error(`[store] refused: ${parsed.refused}`)
          return yield* exit(2)
        }
        const sets = yield* RunSets.Service
        const refused = yield* sets.refusal(parsed)
        if (refused !== null) {
          yield* Console.error(`[store] refused: ${refused}`)
          return yield* exit(2)
        }
        for (const row of yield* sets.runs(parsed, input.limit)) yield* Console.log(runLine(row))
      })
  ).pipe(Command.withDescription("The runs a selector names, newest first"))

  const compareCommand = Command.make("compare", Compare.flags(facts), Compare.handler(facts, true)).pipe(
    Command.withDescription(
      `What moved between two run sets (\`${facts.invocation("compare")}\`); a bare integer is a sweep here`
    )
  )

  const whereCommand = Command.make(
    "where",
    {
      capture: Argument.string("capture").pipe(
        Argument.withDescription("The capture, named in full as the corpus directory is")
      ),
      prep: IdFlags.flag("prep", "prep").pipe(
        Flag.withDescription("The cut to read the capture as of"),
        Flag.optional
      ),
      cases: Flag.boolean("cases").pipe(
        Flag.withDescription("List every case directory, materialised, rather than count them"),
        Flag.withDefault(false)
      )
    },
    (input) =>
      Effect.gen(function*() {
        yield* where(facts, yield* rootPaths, input.capture, {
          prep: Option.getOrUndefined(input.prep),
          cases: input.cases
        })
      })
  ).pipe(Command.withDescription("Every artifact of one capture"))

  const caseDocCommand = Command.make(
    "case-doc",
    {
      first: Argument.string("capture-or-id").pipe(
        Argument.withDescription("The capture, then its case; or a case id alone, in any cited form")
      ),
      second: Argument.string("case").pipe(
        Argument.withDescription("The case of the capture: caseN, a full case id"),
        Argument.optional
      ),
      prep: IdFlags.flag("prep", "prep").pipe(
        Flag.withDescription("The prep to read the case as of; the newest naming it where absent"),
        Flag.optional
      ),
      print: Flag.choice("print", CaseDoc.PRINTS).pipe(
        Flag.withDescription(
          "path: the case directory, document and side rows; flow: the reference ladder; ledger: the side rows"
        ),
        Flag.withDefault("path")
      )
    },
    (input) =>
      Effect.gen(function*() {
        const at = yield* rootPaths
        const prep = Option.getOrUndefined(input.prep)
        const asked: CaseDoc.Input = Option.match(input.second, {
          onNone: () => ({ id: input.first, print: input.print }),
          onSome: (id) => ({ capture: input.first, id, print: input.print })
        })
        const read = yield* CaseDoc.read(at.sessions, prep === undefined ? asked : { ...asked, prep })
        if (read._tag === "refused") {
          yield* Console.error(`[store] refused: ${read.why}`)
          return yield* exit(1)
        }
        for (const line of read.lines) yield* Console.log(line)
      })
  ).pipe(Command.withDescription("One case's document by any cited id: its path, reference ladder or side rows"))

  const deltaCommand = Command.make(
    "delta",
    {
      lane: laneFlag(facts),
      against: Flag.string("against").pipe(
        Flag.withDescription("The lane the cells are green on"),
        Flag.withDefault(facts.defaultLane)
      ),
      limit: limitFlag
    },
    (input) =>
      Effect.gen(function*() {
        if (input.against === input.lane) {
          yield* Console.error(`[store] refused: --lane and --against are both ${input.lane}`)
          return yield* exit(2)
        }
        const delta = yield* Delta.Service
        const population = yield* delta.population(input.lane, input.against)
        const rows = yield* delta.rows(input.lane, input.against)
        const unasked = yield* delta.unasked(input.lane, input.against)
        yield* Console.log(Delta.render(population, rows, unasked, input.limit))
      })
  ).pipe(Command.withDescription("Green there, red here"))

  const rulehits = Command.make(
    "rulehits",
    { limit: Flag.integer("limit").pipe(Flag.withDescription("Runs to measure"), Flag.optional) },
    (input) => backfillRuleHits(Option.getOrElse(input.limit, () => Number.MAX_SAFE_INTEGER))
  ).pipe(Command.withDescription("Count the rule hits of runs never measured"))

  const callsBackfill = Command.make(
    "calls-backfill",
    {
      lane: Flag.string("lane").pipe(
        Flag.withDescription("The lane whose standing runs to measure; every lane where absent"),
        Flag.optional
      )
    },
    (input) =>
      Effect.gen(function*() {
        yield* backfillCalls(yield* rootPaths, Option.getOrUndefined(input.lane))
      })
  ).pipe(Command.withDescription("Measure what the per-call queue lacks: main errors and refusals"))

  const selectorArgument = Argument.string("selector").pipe(
    Argument.withDescription(`The runs: a run id, or ${facts.selectorForms}`)
  )

  const pinCommand = Command.make(
    "pin",
    {
      selector: selectorArgument,
      ticket: Flag.string("ticket").pipe(Flag.withDescription("The ticket that keeps them"), Flag.optional)
    },
    (input) =>
      Effect.gen(function*() {
        const ids = yield* runsNamed(input.selector)
        if (typeof ids === "string") {
          yield* Console.error(`[store] refused: ${ids}`)
          return yield* exit(2)
        }
        const pinned = yield* Effect.flatMap(Pins.Service, (pins) => pins.pin(ids, Option.getOrNull(input.ticket)))
        yield* Console.log(`[store] pinned ${pinned.length} run(s) of ${ids.length} named; the others were pinned already`)
      })
  ).pipe(Command.withDescription("Keep runs and their bundles through retention and the wipe"))

  const unpinCommand = Command.make("unpin", { selector: selectorArgument }, (input) =>
    Effect.gen(function*() {
      const ids = yield* runsNamed(input.selector)
      if (typeof ids === "string") {
        yield* Console.error(`[store] refused: ${ids}`)
        return yield* exit(2)
      }
      const unpinned = yield* Effect.flatMap(Pins.Service, (pins) => pins.unpin(ids))
      yield* Console.log(`[store] unpinned ${unpinned.length} run(s)`)
    })).pipe(Command.withDescription("Let retention take runs a pin kept"))

  const pinsCommand = Command.make("pins", {}, () =>
    Effect.gen(function*() {
      for (const pin of yield* Effect.flatMap(Pins.Service, (pins) => pins.all)) {
        yield* Console.log(`run${pin.runId}  pinned ${pin.pinnedAt}${pin.ticket === null ? "" : `  by ${pin.ticket}`}`)
      }
    })).pipe(Command.withDescription("The pinned runs, newest first"))

  const closeSweepCommand = Command.make(
    "close-sweep",
    { sweep: IdFlags.argument("sweep", "sweep").pipe(Argument.withDescription("The sweep whose process is gone")) },
    (input) =>
      Effect.gen(function*() {
        const closed = yield* SweepClose.close(input.sweep)
        if (closed._tag === "refused") {
          yield* Console.error(`[store] refused: ${closed.why}`)
          return yield* exit(1)
        }
        yield* Console.log(
          `[store] sweep ${input.sweep} closed: ${closed.runs.length} open run(s) closed as abandoned` +
            (closed.job === null ? "" : `, job ${closed.job} closed`)
        )
      })
  ).pipe(Command.withDescription("Close a sweep whose process is gone, and the runs it left open"))

  const closePrepCommand = Command.make(
    "close-prep",
    { prep: IdFlags.argument("prep", "prep").pipe(Argument.withDescription("The prep whose cut is gone")) },
    (input) =>
      Effect.gen(function*() {
        const closed = yield* PrepClose.close(input.prep)
        if (closed._tag === "refused") {
          yield* Console.error(`[store] refused: ${closed.why}`)
          return yield* exit(1)
        }
        yield* Console.log(`[store] prep ${input.prep} closed`)
      })
  ).pipe(Command.withDescription("Finish a prep whose cut is gone"))

  return [
    migrate,
    seed,
    rebuild,
    statusCommand,
    sweepsCommand,
    jobsCommand,
    runsCommand,
    compareCommand,
    whereCommand,
    caseDocCommand,
    deltaCommand,
    rulehits,
    callsBackfill,
    pinCommand,
    unpinCommand,
    pinsCommand,
    closeSweepCommand,
    closePrepCommand
  ] as const
}

/**
 * The store command named `name`: its verbs, then the deployment's `extra`
 * ones, each handed the roots the root command was given.
 */
export const command = <Name extends string, Extra extends ReadonlyArray<Command.Command<any, any, any, any, any>>>(
  facts: CliFacts,
  name: Name,
  extra: (rootPaths: Effect.Effect<Paths, any, any>) => Extra = () => [] as unknown as Extra
) => {
  let lines: ReadonlyArray<string> = []
  const root = rootOf(facts, name, () => lines)
  const rootPaths = Effect.flatMap(root, (shared) => pathsOf(overridesOf(shared)))
  const all = [...verbs(facts, rootPaths), ...extra(rootPaths)] as const
  const width = Math.max(...all.map((verb) => verb.name.length))
  lines = all.map((verb) => `  ${verb.name.padEnd(width)}  ${(verb.description ?? "").split("\n")[0]}`)
  return root.pipe(Command.withSubcommands(all))
}
