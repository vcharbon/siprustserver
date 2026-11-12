/**
 * How a capture replay fits each lane of the catalog: it `fits` a lane whose
 * capabilities hold what a capture replay needs (`scripted-routing`), is
 * `forced` onto one lacking it, which runs it with its results marked forced,
 * and is `none` on a lane with no harness on its SUT's routing path
 * (`backend.mode = "none"`), where no cell runs, forced or not.
 */
import { noRoutingPath } from "@sip/lab-host/lane-checks"
import { Capability, type LaneCatalog, type LaneView } from "@sip/rig"

export type Fit = "fits" | "forced" | "none"

export interface LaneFit {
  readonly lane: string
  readonly fit: Fit
  /** What the lane lacks of what a capture replay needs; [] where it fits. */
  readonly lacks: ReadonlyArray<string>
  /** The line a refusal or a forced run states; null where it fits. */
  readonly line: string | null
}

/** A capture replay on `lane`, forced or not, as {@link Fit} states it. */
export const fitOf = (lane: LaneView.View): LaneFit => {
  if (lane.kind === "hosted" && lane.backendMode === "none") {
    return { lane: lane.id, fit: "none", lacks: [...Capability.PCAP_REPLAY_NEEDS], line: noRoutingPath(lane) }
  }
  const fit = Capability.fit(lane.id, lane.capabilities, Capability.PCAP_REPLAY_NEEDS, "a capture replay", true)
  return fit._tag === "fits"
    ? { lane: lane.id, fit: "fits", lacks: [], line: null }
    : { lane: lane.id, fit: "forced", lacks: fit.lacks, line: fit.line }
}

/** Every lane of `catalog`, in its order, with its fit. */
export const fits = (catalog: LaneCatalog.Catalog): ReadonlyArray<LaneFit> => catalog.lanes.map(fitOf)

export * as PcapLanes from "./pcap-lanes.ts"
