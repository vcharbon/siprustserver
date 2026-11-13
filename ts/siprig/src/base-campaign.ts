/**
 * The base's campaign run for a pack that ships no run verb of its own: every
 * cell its case sources yield (`base-campaign-cells.ts`), on the lanes the
 * run names (every one where it names none) among the workspace's, else the
 * pack's own (`[modules] lanes`, else the `lanes/` of its directory), the
 * built-in simulated lane always among them, through the
 * runner's phases (`@sip/campaign-runner`) under the base's campaign pack and
 * lane hosting. It runs as a campaign job (the row it was launched as, else
 * its own) under the environment's run lock, recorded in the store as the
 * campaign named after the pack, its collectors those the pack's rig path
 * wires on each lane, at the collection level the run names. Nothing is
 * provisioned. The exit code is the runner's: 0 every cell passed, 1 one
 * failed, 2 one errored or a phase did not run.
 */
import { asJob, progressOf } from "@sip/campaign-runner/job"
import { closedOnExit, exitCodeOf, loadOf, stamp } from "@sip/campaign-runner/command"
import { exclusively } from "@sip/campaign-runner/exclusive"
import { lanesIn, lanesOf, resolve } from "@sip/campaign-runner/lanes"
import { phases } from "@sip/campaign-runner/phases"
import * as Record from "@sip/campaign-runner/record"
import { run as runPhases, PhaseFailed } from "@sip/campaign-runner/run"
import { recordOf, type Tagging } from "@sip/campaign-runner/selection"
import { sinkOf } from "@sip/campaign-runner/setup-phase"
import { Collectors, LaneHosting, Provenance, ReplayIdentity } from "@sip/corpus"
import { LaneCatalog } from "@sip/rig"
import * as Pack from "@sip/rig/pack"
import { CampaignRuns, JobEvents, Jobs, Sessions } from "@sip/store"
import * as Arr from "effect/Array"
import * as Console from "effect/Console"
import * as Effect from "effect/Effect"
import * as FileSystem from "effect/FileSystem"
import * as Result from "effect/Result"
import * as path from "node:path"
import { compile, selectionOf } from "./base-campaign-cells.ts"
import { CAMPAIGN, REPORT } from "./base-campaign-pack.ts"
import { deploymentOf, hostingOf, readingOf } from "./base-lane-hosting.ts"

/** What a base campaign run is asked. */
export interface Options {
  /** The pack's directory, absolute. */
  readonly pack: string
  /** The workspace's root: its lane and part files, and its plug-ins after the pack's; none where undefined. */
  readonly workspace: string | undefined
  /** The lanes to run on, comma-separated; every lane of the pack where undefined. */
  readonly lanes: string | undefined
  /** The run's directory; a stamped one under `runs` where undefined. */
  readonly out: string | undefined
  /** Where runs are written. */
  readonly runs: string
  readonly junit: string | undefined
  /** The campaign job row the run closes; its own where undefined. */
  readonly job: number | undefined
  /** The environment the run is recorded and locked under. */
  readonly env: string
  /** The site the report's links point at. */
  readonly site: string
  /** The replay binary a simulated cell runs. */
  readonly replay: string
  /** The `pivot-schema` binary the compile's lint runs. */
  readonly pivotSchema: string
  /** The collection level the run names; the default where undefined. */
  readonly collection: string | undefined
}

/** The pack manifest at `pack`, or why it is refused. */
export const manifestOf = (pack: string) =>
  Effect.gen(function*() {
    const file = path.join(pack, Pack.FILE)
    const text = yield* (yield* FileSystem.FileSystem).readFileString(file).pipe(
      Effect.mapError(() => `${file}: a pack holds ${Pack.FILE} at its root`)
    )
    return yield* Effect.fromResult(Result.mapError(Pack.parse(file, text), (refusal) => refusal.message))
  })

/** The root the pack's lane and part files are under: the parent of its `[modules] lanes`, else its own directory. */
export const lanesRootOf = (pack: string, manifest: Pack.Pack): string =>
  manifest.modules?.lanes === undefined ? pack : path.dirname(path.resolve(pack, manifest.modules.lanes))

/** What each compiled case says of itself, by ref: its title. */
const aboutOf = (selection: Effect.Success<ReturnType<typeof selectionOf>>) =>
  Object.fromEntries(selection.selected.flatMap((file) => (file.cells ?? []).map((cell) => [cell.case, { title: cell.title }])))

const NO_TAGS: Tagging = { declared: { file: "", tags: {}, tagCases: [] }, allow: [] }

/** The base campaign run `options` asks for: its exit code, a refusal before it opens said and answered 2. */
export const run = (options: Options) =>
  Effect.gen(function*() {
    const refuse = (reason: string) => Effect.as(Console.error(`${CAMPAIGN.verb}: ${reason}`), 2 as const)
    const manifest = yield* manifestOf(options.pack).pipe(Effect.result)
    if (Result.isFailure(manifest)) return yield* refuse(manifest.failure)
    const lanesRoot = options.workspace ?? lanesRootOf(options.pack, manifest.success)
    const catalog = yield* LaneCatalog.load(readingOf(options.pack), LaneCatalog.dirsUnder(lanesRoot)).pipe(Effect.result)
    if (Result.isFailure(catalog)) return yield* refuse(catalog.failure.message)
    const named = options.lanes === undefined ? lanesOf("", catalog.success) : lanesIn("", options.lanes, catalog.success)
    if (typeof named === "string") return yield* refuse(named)
    if (!Arr.isReadonlyArrayNonEmpty(named)) return yield* refuse("the pack states no lane")
    const lanes = named
    const selection = yield* selectionOf(options.pack).pipe(Effect.result)
    if (Result.isFailure(selection)) return yield* refuse(selection.failure.message)
    const capture = manifest.success.name
    const root = path.resolve(options.out ?? path.join(options.runs, `${stamp()}-${options.env}`))
    const picked = lanes.map((lane) => LaneCatalog.find(catalog.success, resolve("", catalog.success, lane))!)
    const jobs = yield* Jobs.Service
    const events = yield* JobEvents.Service
    const store = yield* CampaignRuns.Service
    const sessions = yield* Sessions.Service
    yield* Console.log(`${CAMPAIGN.verb} of ${capture} in ${options.env} on ${lanes.join(", ")}: ${root}`)
    const launch = { capture, lanes, case: null, subcase: null, rebuild: false, allow: [] }
    const body = (job: number | undefined) =>
      Effect.gen(function*() {
        const record = yield* Record.open(store, lanes, {
          capture,
          columns: { target: {} },
          sessions: sessions.dir,
          dir: root,
          seed: null,
          scope: null,
          selection: recordOf(NO_TAGS, picked, false),
          provenance: yield* Provenance.current,
          ...(job === undefined ? {} : { jobId: job }),
          identity: (lane) => ReplayIdentity.current(lane, loadOf(LaneCatalog.find(catalog.success, lane)!)),
          idOf: (lane) => resolve("", catalog.success, lane)
        }).pipe(Effect.mapError((e) => new Error(`the store refused the run: ${String(e)}`)))
        const ran = phases({ campaign: CAMPAIGN, hosting: hostingOf(options.replay), report: REPORT, store: Effect.succeed(store) }, {
          catalog: catalog.success,
          routing: "",
          quick: false,
          individually: false,
          scope: null,
          coverage: false,
          force: false,
          tagging: NO_TAGS,
          root,
          junit: options.junit === undefined ? undefined : path.resolve(options.junit),
          site: options.site,
          record,
          setup: { instances: [], sink: sinkOf(job === undefined ? undefined : { id: job, events }) },
          ...(job === undefined ? {} : { progress: progressOf(jobs, job) })
        }, {
          provision: Effect.void,
          compile: Effect.tap(
            compile(selection.success, path.join(root, "cells"), options.pivotSchema),
            () => store.describe(record.id, aboutOf(selection.success)).pipe(
              Effect.mapError((e) => new PhaseFailed({ phase: "compile", reason: `record the cases: ${String(e)}` }))
            )
          ),
          laneKept: () => Effect.void,
          ported: Effect.succeed([]),
          laneUnrunnable: () => undefined,
          targetUnrunnable: () => undefined
        })
        return yield* closedOnExit(store, record, runPhases(lanes, ran, CAMPAIGN.own))
      })
    const program = asJob(CAMPAIGN.verb, options.job, { launch, createdBy: options.env }, exclusively(CAMPAIGN, options.env), body)
    return yield* exitCodeOf(CAMPAIGN.verb, program).pipe(
      Effect.provide(Collectors.layerOf(catalog.success.rig, catalog.success.lanes.map((lane) => lane.loaded))),
      Effect.provide(LaneHosting.layerWith(deploymentOf(catalog.success))),
      Effect.provideService(Collectors.Level, options.collection)
    )
  })

export * as BaseCampaign from "./base-campaign.ts"
