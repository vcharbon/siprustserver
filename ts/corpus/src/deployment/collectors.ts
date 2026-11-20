/**
 * `Collectors` — the collectors a deployment's rig path runs around a job and
 * around each batch (`@sip/rig` `CollectorPlan`, ADR-0060 appendix §3, §5,
 * §10): the instances a batch on a lane runs, its cell-scope ones per cell,
 * and those a job over its lanes
 * runs, pre-checks among them (`PreCheck.isPreCheck`; an `on-demand` one only
 * where the job asks for it), each instance's directory under the one its
 * caller names, as the job's collection level ({@link Level}) sets them; the
 * pre-checks a job must run that its lanes leave unwired (`PreCheck.unwiredOf`); the
 * judges a batch runs over its exports alone (`@sip/rig` `JudgePlan`); and the
 * instance an on-demand run of one collector takes on a lane.
 *
 * The default runs none. A deployment states its rig path, its lanes, as the
 * rig loaded them, and its workspace's `[collection]` with {@link layerOf}; a
 * lane it did not load wires no collector.
 */
import { Collection, CollectorPlan, type CollectorRun, JudgePlan, type Lanes, PreCheck, type RigPath, type ScopeJudging } from "@sip/rig"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"
import * as Result from "effect/Result"
import * as path from "node:path"

/** The collection level the job at hand names (`--collection`); absent, the workspace's default. */
export const Level: Context.Reference<string | undefined> = Context.Reference("@sip/corpus/Collectors/Level", {
  defaultValue: (): string | undefined => undefined
})

export interface Interface {
  /** The collectors a batch on `lane` runs, their directories under `dir`, the batch's own attachments under `holder`. */
  readonly batch: (lane: string, dir: string, holder: string) => Effect.Effect<ReadonlyArray<CollectorRun.Instance>>
  /** The cell-scope collectors a batch on `lane` runs, each cell's scope a directory under `root`; none where absent. */
  readonly cell?: (lane: string, root: string) => Effect.Effect<CollectorPlan.CellPlan | undefined>
  /**
   * The collectors a job over `lanes` runs, their directories under `dir`, the
   * job's own attachments under `holder`; the collectors `asked` names among
   * them, whatever the level sets them.
   */
  readonly job: (
    lanes: ReadonlyArray<string>,
    dir: string,
    holder: string,
    asked?: ReadonlyArray<string>
  ) => Effect.Effect<ReadonlyArray<CollectorRun.Instance>>
  /** The judges a batch on `lane`, whose directory is `holder`, runs over its exports alone; none where absent or it runs none. */
  readonly exportJudges?: (lane: string, holder: string) => Effect.Effect<ScopeJudging.ExportJudges | undefined>
  /**
   * The pre-checks a job over `lanes` must run that a lane leaves unwired
   * (`PreCheck.unwiredOf`): the required ones, of either scope, a real-clock
   * lane names no part of, and those `asked` names that a lane excludes.
   */
  readonly unwired: (lanes: ReadonlyArray<string>, asked?: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<PreCheck.Unwired>>
  /** The instance an on-demand run of the collector `name` takes on `lane`, or why it cannot run there. */
  readonly onDemand: (
    name: string,
    lane: string,
    dir: string,
    holder: string
  ) => Effect.Effect<Result.Result<CollectorRun.Instance, string>>
  /** The collectors an on-demand run on `lane` may ask for, by name. */
  readonly onDemandOn: (lane: string) => Effect.Effect<ReadonlyArray<string>>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Collectors") {}

/** Whether `batch`, what {@link batchOf} read, runs or names anything around a batch. */
export const wiresAny = (batch: {
  readonly instances: ReadonlyArray<CollectorRun.Instance>
  readonly exportJudges: ScopeJudging.ExportJudges | undefined
  readonly cell: CollectorPlan.CellPlan | undefined
}): boolean =>
  batch.instances.length > 0 || batch.exportJudges !== undefined ||
  (batch.cell !== undefined && (batch.cell.runs.length > 0 || batch.cell.unrun.length > 0))

/**
 * The directory under a batch's collectors' where each cell's scope has its
 * own: no collector is so named, its name not being one a collector takes.
 */
export const CELLS_DIR = "_cells"

/**
 * What the rig runs around a batch on `lane` whose directory is `holder`: its
 * collectors' instances, under `dir`, its export judges, and its cell-scope
 * collectors, each cell's under `dir`'s {@link CELLS_DIR}.
 */
export const batchOf = (seam: Interface, lane: string, dir: string, holder: string) =>
  Effect.all({
    instances: seam.batch(lane, dir, holder),
    exportJudges: seam.exportJudges === undefined ? Effect.succeed(undefined) : seam.exportJudges(lane, holder),
    cell: seam.cell === undefined ? Effect.succeed(undefined) : seam.cell(lane, path.join(dir, CELLS_DIR))
  })

/** A deployment that wires no collector. */
export const NONE: Interface = {
  batch: () => Effect.succeed([]),
  job: () => Effect.succeed([]),
  unwired: () => Effect.succeed([]),
  onDemand: (name) => Effect.succeed(Result.fail(`no collector of the path is named ${name}`)),
  onDemandOn: () => Effect.succeed([])
}

/** The default: no collector runs. */
export const layer = Layer.succeed(Service, Service.of(NONE))

/** The settings of the job at hand: `collection`'s level it names, else its default. */
const settings = (collection: Collection.Collection | undefined) =>
  Effect.gen(function*() {
    return Collection.settingsOf(collection, yield* Level)
  })

/** The collectors `rig` plans over `lanes`, each lane by its id, as the level `collection` defines sets them. */
export const of = (
  rig: RigPath.RigPath,
  lanes: ReadonlyArray<Lanes.Loaded>,
  collection?: Collection.Collection
): Interface => {
  const byId = new Map(lanes.map((loaded) => [loaded.id, loaded]))
  const loaded = (named: ReadonlyArray<string>) => named.flatMap((lane) => byId.get(lane) ?? [])
  return {
    batch: (lane, dir, holder) =>
      Effect.map(settings(collection), (set) => {
        const found = byId.get(lane)
        return found === undefined ? [] : CollectorPlan.batchPlanOf(rig, found, dir, holder, set)
      }),
    cell: (lane, root) =>
      Effect.map(settings(collection), (set) => {
        const found = byId.get(lane)
        return found === undefined ? undefined : CollectorPlan.cellPlanOf(rig, found, root, set)
      }),
    job: (named, dir, holder, asked) =>
      Effect.map(settings(collection), (set) => CollectorPlan.jobPlanOf(rig, loaded(named), dir, holder, asked, set)),
    exportJudges: (lane, holder) =>
      Effect.sync(() => {
        const found = byId.get(lane)
        const planned = found === undefined ? undefined : JudgePlan.exportJudgesOn(rig, found, holder)
        return planned === undefined || planned.judges.length === 0 ? undefined : planned
      }),
    unwired: (named, asked) => Effect.sync(() => PreCheck.unwiredOf(rig, loaded(named), asked)),
    onDemand: (name, lane, dir, holder) =>
      Effect.map(settings(collection), (set) => {
        const found = byId.get(lane)
        return found === undefined
          ? Result.fail(`the lane ${lane} is not one the deployment loaded`)
          : CollectorPlan.onDemandOf(rig, name, found, dir, holder, set)
      }),
    onDemandOn: (lane) =>
      Effect.map(settings(collection), (set) => {
        const found = byId.get(lane)
        return found === undefined ? [] : CollectorPlan.onDemandOn(rig, found, set)
      })
  }
}

/** A deployment's own collectors: those `rig` plans over `lanes`, as `collection`'s levels set them. */
export const layerOf = (
  rig: RigPath.RigPath,
  lanes: ReadonlyArray<Lanes.Loaded>,
  collection?: Collection.Collection
): Layer.Layer<Service> => Layer.succeed(Service, Service.of(of(rig, lanes, collection)))
