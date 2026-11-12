/**
 * The ON-DEMAND collectors a cell's call details offer and run (upstream
 * ADR-0060 appendix §5, `@sip/lab-host` `on-demand.ts`): those the
 * deployment's level sets `on-demand` and wires on the cell's lane, run on
 * the one cell the page names, addressed as the study addresses it (capture,
 * run name, cell name) beside the case result it lands under, its lane, and
 * its case result, which the store's run row behind the run name must keep
 * the cell as. What it attaches lands on the cell, where the page reads it;
 * its lines are answered to the page and never merged into the cell.
 */
import { OnDemand } from "@sip/lab-host"
import type { Verification } from "@sip/contracts"
import type { Collectors, Roots } from "@sip/corpus"
import type { Attachments } from "@sip/store"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import { FileSystem } from "effect/FileSystem"
import * as Layer from "effect/Layer"
import { Path } from "effect/Path"
import { CollectRefused } from "./api.ts"
import { CampaignRuns } from "./campaign-runs.ts"
import { ViewerConfig } from "./config.ts"
import * as RunDirs from "./run-dirs.ts"
import { areSegments } from "./segment.ts"
import { captureStem } from "./triage.ts"

/** The one cell an on-demand run is asked of, and the collector. */
export interface Asked {
  readonly collector: string
  readonly capture: string
  /** The run's name as the study lists it (`run<id>`). */
  readonly session: string
  /** The cell's directory name. */
  readonly cell: string
  /** The case result it lands under: its run row and case id. */
  readonly runId: number
  readonly caseId: string
  /** The cell's lane, by the catalog id its run row states and the page shows. */
  readonly lane: string
}

/** What the run answered of the cell. */
export interface Answered {
  readonly verifications: ReadonlyArray<Verification.Verification>
  readonly attached: number
  /** The collectors whose `close` left the lab changed. */
  readonly leftChanged: ReadonlyArray<string>
}

export interface Interface {
  /** The collectors an on-demand run may ask for on `lane` (its catalog id), by name; none on a lane the deployment does not load. */
  readonly offered: (lane: string) => Effect.Effect<ReadonlyArray<string>>
  readonly run: (asked: Asked) => Effect.Effect<Answered, CollectRefused>
}

export class Service extends Context.Service<Service, Interface>()("@sip/site/Collect") {}

type Needs = Collectors.Service | Attachments.Service | CampaignRuns.Service | FileSystem | Roots.Service

export const layer = Layer.effect(
  Service,
  Effect.gen(function*() {
    const config = yield* ViewerConfig.Service
    const fs = yield* FileSystem
    const path = yield* Path
    const context = yield* Effect.context<Needs>()
    const refused = (reason: string) => new CollectRefused({ reason })

    /** The run directory of `asked`'s cell, where the page may address it. */
    const runDirOf = (asked: Asked) =>
      Effect.gen(function*() {
        const triage = config.triage
        if (triage === undefined) return yield* refused("this deployment states no session tree")
        if (!areSegments(asked.capture, asked.session, asked.cell)) {
          return yield* refused(`${asked.capture}/${asked.session}/${asked.cell} is no cell this site addresses`)
        }
        const runDir = yield* RunDirs.runDirOf(path.join(triage.sessions, captureStem(asked.capture)), asked.session).pipe(
          Effect.provideService(FileSystem, fs),
          Effect.provideService(Path, path)
        )
        const held = yield* fs.exists(path.join(runDir, asked.cell)).pipe(Effect.orElseSucceed(() => false))
        return held ? runDir : yield* refused(`no cell ${asked.cell} under the run ${asked.session} of ${asked.capture}`)
      })

    /** `asked` refused where the run row behind `runDir` keeps its cell as another case result or on another lane than the page says. */
    const keptAsAsked = (asked: Asked, runDir: string) =>
      Effect.gen(function*() {
        const kept = yield* Effect.flatMap(CampaignRuns.Service, (results) => results.keptCells(runDir)).pipe(
          Effect.provide(context),
          Effect.mapError((error) => refused(`the store did not say what the run ${asked.session} keeps: ${error.message}`))
        )
        const one = kept.find((cell) => cell.cell === asked.cell)
        if (one === undefined) return yield* refused(`the run ${asked.session} of ${asked.capture} keeps no case result of the cell ${asked.cell}`)
        if (one.runId !== asked.runId || one.caseId !== asked.caseId || one.lane !== asked.lane) {
          return yield* refused(
            `the run ${asked.session} keeps ${asked.cell} as ${one.caseId} of run ${one.runId} on ${one.lane}, ` +
              `not ${asked.caseId} of run ${asked.runId} on ${asked.lane}`
          )
        }
      })

    const run = Effect.fn("Collect.run")(function*(asked: Asked) {
      const runDir = yield* runDirOf(asked)
      yield* keptAsAsked(asked, runDir)
      const ran = yield* OnDemand.run({
        collector: asked.collector,
        lane: asked.lane,
        runDir,
        cells: [{ cell: asked.cell, runId: asked.runId, caseId: asked.caseId }]
      }).pipe(
        Effect.provide(context),
        Effect.mapError((error) => refused(error.reason))
      )
      const answers = ran.cells[0]?.answers
      return {
        verifications: answers?.verifications ?? [],
        attached: answers?.attachments.length ?? 0,
        leftChanged: ran.leftChanged
      } satisfies Answered
    })

    return Service.of({
      offered: (lane) => OnDemand.offered(lane).pipe(Effect.provide(context)),
      run
    })
  })
)

export * as Collect from "./collect.ts"
