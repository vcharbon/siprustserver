/**
 * Which value a replay knob takes: the one stated, else the request row's,
 * else the lane's own default, else the generic one — knob by knob, so a
 * stated value equal to the generic default still wins over the lane's.
 */
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import { LaneHosting } from "../../src/deployment/index.js"
import { DEFAULT_BATCH, DEFAULT_CONCURRENCY, DEFAULT_STARTS_PER_SECOND } from "../../src/knobs.js"
import * as KnobResolution from "../../src/replay/knob-resolution.js"

const WALL: LaneHosting.Lane = {
  clock: "real",
  exclusive: true,
  defaults: { concurrency: 100, batchSize: 4000, startsPerSecond: 10 }
}

const hosting = LaneHosting.layerWith({
  lanes: ["paused", "wall"],
  lane: (lane) =>
    lane === "paused"
      ? Effect.succeed(LaneHosting.NEUTRAL_LANE)
      : lane === "wall"
      ? Effect.succeed(WALL)
      : Effect.fail(new LaneHosting.LaneUnstated({ lane, known: ["paused", "wall"] })),
  standBatch: () => Effect.succeed(undefined),
  standSut: () => Effect.succeed(undefined),
  preflight: () => Effect.succeed(null)
})

describe("KnobResolution", () => {
  it("names the generic defaults", () => {
    expect(KnobResolution.GENERIC).toEqual({
      concurrency: DEFAULT_CONCURRENCY,
      batchSize: DEFAULT_BATCH,
      startsPerSecond: DEFAULT_STARTS_PER_SECOND
    })
  })

  it("takes a lane's own default for each knob it states, the generic one for the rest", () => {
    expect(KnobResolution.ofLane(WALL)).toEqual({ concurrency: 100, batchSize: 4000, startsPerSecond: 10 })
    expect(KnobResolution.ofLane({ ...LaneHosting.NEUTRAL_LANE, defaults: { startsPerSecond: 3 } })).toEqual({
      ...KnobResolution.GENERIC,
      startsPerSecond: 3
    })
    expect(KnobResolution.ofLane(LaneHosting.NEUTRAL_LANE)).toEqual(KnobResolution.GENERIC)
  })

  it("resolves stated, then the request row, then the lane, then the generic default, knob by knob", () => {
    const lane = KnobResolution.ofLane(WALL)
    expect(KnobResolution.resolve({}, undefined, lane)).toEqual({ concurrency: 100, batchSize: 4000, startsPerSecond: 10 })
    expect(KnobResolution.resolve({}, { concurrency: 50 }, lane)).toEqual({
      concurrency: 50,
      batchSize: 4000,
      startsPerSecond: 10
    })
    expect(KnobResolution.resolve({ concurrency: 8, startsPerSecond: 0 }, { concurrency: 50, batchSize: 200 }, lane))
      .toEqual({ concurrency: 8, batchSize: 200, startsPerSecond: 0 })
    expect(KnobResolution.resolve({}, undefined, KnobResolution.GENERIC)).toEqual(KnobResolution.GENERIC)
  })

  it("reads a lane's defaults off the hosting; a lane it does not state takes the generic ones", async () => {
    const read = (lane: string) => Effect.runPromise(KnobResolution.forLane(lane).pipe(Effect.provide(hosting)))
    expect(await read("wall")).toEqual({ concurrency: 100, batchSize: 4000, startsPerSecond: 10 })
    expect(await read("paused")).toEqual(KnobResolution.GENERIC)
    expect(await read("gone")).toEqual(KnobResolution.GENERIC)
  })
})
