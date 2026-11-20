/**
 * The lanes the deployment runs, each with the ceilings it clamps a request's
 * knobs to and the knobs it defaults, read off the lane catalog through
 * what each lane states to the orchestrator (`@sip/lab-host` `lane-hosting.ts`):
 * what the request builder shows beside its knob fields. A request's knob
 * nobody states is the lane's default, else the generic one.
 */
import { laneOf } from "@sip/lab-host/lane-hosting"
import { LaneCatalog } from "@sip/rig"
import { DEFAULT_CLEANUP, DEFAULT_MEMORY_MAX } from "@sip/corpus/knobs"
import * as KnobResolution from "@sip/corpus/replay/knob-resolution"
import type { Requests } from "@sip/store"

export interface Lane {
  readonly lane: string
  readonly exclusive: boolean
  readonly cellCeiling: number | null
  readonly startCeiling: number | null
  /** What a request on the lane takes where nobody states a knob. */
  readonly defaults: KnobResolution.Knobs
}

/** Every lane, in catalog order, with its ceilings and its defaults. */
export const lanes = (catalog: LaneCatalog.Catalog): ReadonlyArray<Lane> =>
  catalog.lanes.map((lane) => {
    const hosted = laneOf(lane)
    return {
      lane: lane.id,
      exclusive: hosted.exclusive,
      cellCeiling: hosted.cellCeiling ?? null,
      startCeiling: hosted.startCeiling ?? null,
      defaults: KnobResolution.ofLane(hosted)
    }
  })

/** The generic knobs: what a request takes where neither the page nor its lane states one. */
export const DEFAULT_KNOBS: Requests.Knobs = {
  ...KnobResolution.GENERIC,
  memoryMax: DEFAULT_MEMORY_MAX,
  cleanup: DEFAULT_CLEANUP
}

/** The knobs a request on `lane` of `catalog` is created with: each as `stated`, else the lane's default, else the generic one. */
export const knobsOn = (catalog: LaneCatalog.Catalog, lane: string, stated: Partial<Requests.Knobs>): Requests.Knobs => {
  const found = LaneCatalog.find(catalog, lane)
  const knobs = KnobResolution.resolve(
    stated,
    undefined,
    found === undefined ? KnobResolution.GENERIC : KnobResolution.ofLane(laneOf(found))
  )
  return {
    concurrency: Math.max(1, knobs.concurrency),
    batchSize: Math.max(1, knobs.batchSize),
    memoryMax: stated.memoryMax === undefined || stated.memoryMax === "" ? DEFAULT_KNOBS.memoryMax : stated.memoryMax,
    startsPerSecond: Math.max(0, knobs.startsPerSecond),
    cleanup: stated.cleanup ?? DEFAULT_KNOBS.cleanup
  }
}

export * as Lanes from "./lanes.ts"
