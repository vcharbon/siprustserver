/**
 * What a lane states to the orchestrator (`@sip/corpus` `LaneHosting.Lane`),
 * off its catalog view: the built-in simulated lane is the neutral lane, each
 * cell on its own on a paused clock; a hosted lane runs on the real clock,
 * exclusive unless its SIP target says otherwise, with the target's ceilings,
 * default knobs and lifecycle. Its capabilities are the catalog's, sorted.
 */
import { LaneHosting } from "@sip/corpus"
import type { LaneView } from "@sip/rig"

/** What `lane` states to the orchestrator. */
export const laneOf = (lane: LaneView.View): LaneHosting.Lane => {
  const capabilities = [...lane.capabilities].sort()
  if (lane.kind === "simulated") return { ...LaneHosting.NEUTRAL_LANE, capabilities }
  const sip = lane.sip
  const knobs = sip.knobs
  const defaults: LaneHosting.LaneDefaults | undefined = knobs === undefined ? undefined : {
    ...(knobs.concurrency === undefined ? {} : { concurrency: knobs.concurrency }),
    ...(knobs.batch_size === undefined ? {} : { batchSize: knobs.batch_size }),
    ...(knobs.starts_per_second === undefined ? {} : { startsPerSecond: knobs.starts_per_second })
  }
  return {
    clock: "real",
    exclusive: sip.exclusive ?? true,
    ...(sip.ceilings?.cells === undefined ? {} : { cellCeiling: sip.ceilings.cells }),
    ...(sip.ceilings?.starts_per_second === undefined ? {} : { startCeiling: sip.ceilings.starts_per_second }),
    ...(defaults === undefined ? {} : { defaults }),
    sut: sip.lifecycle,
    capabilities
  }
}
