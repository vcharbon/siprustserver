/**
 * `Collectors` — the collectors a deployment's rig path runs around a job and
 * around each batch (`@sip/rig` `CollectorPlan`, ADR-0060 appendix §3, §5):
 * the instances a batch on a lane runs, and those a job over its lanes runs,
 * pre-checks among them (`PreCheck.isPreCheck`; an `on-demand` one only where
 * the job asks for it), each instance's directory under the one its caller
 * names; the required pre-checks a job's lanes leave unwired
 * (`PreCheck.unwiredOf`); and the judges a batch runs over its exports alone
 * (`@sip/rig` `JudgePlan`).
 *
 * The default runs none. A deployment states its rig path and its lanes, as
 * the rig loaded them, with {@link layerOf}; a lane it did not load wires no
 * collector.
 */
import { CollectorPlan, type CollectorRun, JudgePlan, type Lanes, PreCheck, type RigPath, type ScopeJudging } from "@sip/rig"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"

export interface Interface {
  /** The collectors a batch on `lane` runs, their directories under `dir`, the batch's own attachments under `holder`. */
  readonly batch: (lane: string, dir: string, holder: string) => Effect.Effect<ReadonlyArray<CollectorRun.Instance>>
  /**
   * The collectors a job over `lanes` runs, their directories under `dir`, the
   * job's own attachments under `holder`; the `on-demand` ones `asked` names
   * among them.
   */
  readonly job: (
    lanes: ReadonlyArray<string>,
    dir: string,
    holder: string,
    asked?: ReadonlyArray<string>
  ) => Effect.Effect<ReadonlyArray<CollectorRun.Instance>>
  /** The judges a batch on `lane`, whose directory is `holder`, runs over its exports alone; none where absent or it runs none. */
  readonly exportJudges?: (lane: string, holder: string) => Effect.Effect<ScopeJudging.ExportJudges | undefined>
  /** The required pre-checks, of either scope, that a real-clock lane of `lanes` names no part of. */
  readonly unwired: (lanes: ReadonlyArray<string>) => Effect.Effect<ReadonlyArray<PreCheck.Unwired>>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Collectors") {}

/** What the rig runs at the end of a batch on `lane` whose directory is `holder`: its collectors' instances, under `dir`, and its export judges. */
export const batchOf = (seam: Interface, lane: string, dir: string, holder: string) =>
  Effect.all({
    instances: seam.batch(lane, dir, holder),
    exportJudges: seam.exportJudges === undefined ? Effect.succeed(undefined) : seam.exportJudges(lane, holder)
  })

/** The default: no collector runs. */
export const layer = Layer.succeed(
  Service,
  Service.of({ batch: () => Effect.succeed([]), job: () => Effect.succeed([]), unwired: () => Effect.succeed([]) })
)

/** The collectors `rig` plans over `lanes`, each lane by its id. */
export const of = (rig: RigPath.RigPath, lanes: ReadonlyArray<Lanes.Loaded>): Interface => {
  const byId = new Map(lanes.map((loaded) => [loaded.id, loaded]))
  const loaded = (named: ReadonlyArray<string>) => named.flatMap((lane) => byId.get(lane) ?? [])
  return {
    batch: (lane, dir, holder) =>
      Effect.sync(() => {
        const found = byId.get(lane)
        return found === undefined ? [] : CollectorPlan.batchPlanOf(rig, found, dir, holder)
      }),
    job: (named, dir, holder, asked) => Effect.sync(() => CollectorPlan.jobPlanOf(rig, loaded(named), dir, holder, asked)),
    exportJudges: (lane, holder) =>
      Effect.sync(() => {
        const found = byId.get(lane)
        const planned = found === undefined ? undefined : JudgePlan.exportJudgesOn(rig, found, holder)
        return planned === undefined || planned.judges.length === 0 ? undefined : planned
      }),
    unwired: (named) => Effect.sync(() => PreCheck.unwiredOf(rig, loaded(named)))
  }
}

/** A deployment's own collectors: those `rig` plans over `lanes`. */
export const layerOf = (rig: RigPath.RigPath, lanes: ReadonlyArray<Lanes.Loaded>): Layer.Layer<Service> =>
  Layer.succeed(Service, Service.of(of(rig, lanes)))
