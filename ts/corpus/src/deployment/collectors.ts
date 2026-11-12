/**
 * `Collectors` — the collectors a deployment's rig path runs around a job and
 * around each batch (`@sip/rig` `CollectorPlan`, ADR-0060 appendix §3, §5):
 * the instances a batch on a lane runs, and those a job over its lanes runs,
 * each instance's directory under the one its caller names.
 *
 * The default runs none. A deployment states its rig path and its lanes, as
 * the rig loaded them, with {@link layerOf}; a lane it did not load wires no
 * collector.
 */
import { CollectorPlan, type CollectorRun, type Lanes, type RigPath } from "@sip/rig"
import * as Context from "effect/Context"
import * as Effect from "effect/Effect"
import * as Layer from "effect/Layer"

export interface Interface {
  /** The collectors a batch on `lane` runs, their directories under `dir`, the batch's own attachments under `holder`. */
  readonly batch: (lane: string, dir: string, holder: string) => Effect.Effect<ReadonlyArray<CollectorRun.Instance>>
  /** The collectors a job over `lanes` runs, their directories under `dir`, the job's own attachments under `holder`. */
  readonly job: (lanes: ReadonlyArray<string>, dir: string, holder: string) => Effect.Effect<ReadonlyArray<CollectorRun.Instance>>
}

export class Service extends Context.Service<Service, Interface>()("@sip/corpus/Collectors") {}

/** The default: no collector runs. */
export const layer = Layer.succeed(
  Service,
  Service.of({ batch: () => Effect.succeed([]), job: () => Effect.succeed([]) })
)

/** The collectors `rig` plans over `lanes`, each lane by its id. */
export const of = (rig: RigPath.RigPath, lanes: ReadonlyArray<Lanes.Loaded>): Interface => {
  const byId = new Map(lanes.map((loaded) => [loaded.id, loaded]))
  return {
    batch: (lane, dir, holder) =>
      Effect.sync(() => {
        const loaded = byId.get(lane)
        return loaded === undefined ? [] : CollectorPlan.batchPlanOf(rig, loaded, dir, holder)
      }),
    job: (named, dir, holder) =>
      Effect.sync(() =>
        CollectorPlan.jobPlanOf(rig, named.flatMap((lane) => byId.get(lane) ?? []), dir, holder)
      )
  }
}

/** A deployment's own collectors: those `rig` plans over `lanes`. */
export const layerOf = (rig: RigPath.RigPath, lanes: ReadonlyArray<Lanes.Loaded>): Layer.Layer<Service> =>
  Layer.succeed(Service, Service.of(of(rig, lanes)))
