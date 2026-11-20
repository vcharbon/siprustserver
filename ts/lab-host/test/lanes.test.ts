/**
 * The box's lane checks and the batch-spec over the rig's lane view: the
 * simulated lane is always ready, the pack's guard answers before any port is
 * read, a hosted lane with no backend on its routing path is refused naming
 * its mode, and the spec carries the pack's wiring beside its own fields.
 */
import { NodeServices } from "@effect/platform-node"
import { LaneView } from "@sip/rig"
import * as Effect from "effect/Effect"
import { describe, expect, it } from "vitest"
import * as LaneChecks from "../src/lane-checks.js"
import * as ReplayBatch from "../src/replay-batch.js"
import * as RigWiring from "../src/rig-wiring.js"

/** A hosted lane as the rig views one, its backend served at `backendListen`, none where it is undefined. */
const hosted = (backendListen: string | undefined): LaneView.Hosted => ({
  ...LaneView.simulatedBuiltIn("lab-1", {}),
  kind: "hosted",
  file: "/lanes/lab-1.toml",
  backendMode: backendListen === undefined ? "none" : "served",
  backendListen,
  sip: {
    name: "lab",
    file: "/lanes/lab-1.toml",
    description: "the lab SUT",
    lifecycle: "standing",
    ingress: "127.0.0.1:1",
    endpoint_host: "127.0.0.1",
    http_budget: { new_call_ms: 1000, follow_up_ms: 1000, transit_ms: 100 }
  }
})

const guarded = (said: string | null, asked: Array<string>): LaneChecks.Checks<LaneView.View> => ({
  hints: () => ({}),
  guard: (lane) => Effect.sync(() => (asked.push(lane.id), said))
})

const run = <A>(effect: Effect.Effect<A, never, NodeServices.NodeServices>) =>
  Effect.runPromise(Effect.provide(effect, NodeServices.layer))

describe("lane checks", () => {
  it("the simulated lane is ready, its guard never asked", async () => {
    const asked: Array<string> = []
    const lane = LaneView.simulatedBuiltIn("sim", {})
    expect(await run(LaneChecks.refusal(guarded("another lab's", asked), lane, {}))).toBeNull()
    expect(asked).toEqual([])
  })

  it("the pack's guard answers a hosted lane first", async () => {
    const asked: Array<string> = []
    expect(await run(LaneChecks.refusal(guarded("another lab's", asked), hosted("127.0.0.1:2"), {}))).toBe("another lab's")
    expect(asked).toEqual(["lab-1"])
  })

  it("a hosted lane with no harness on its routing path is refused naming its mode and file", async () => {
    expect(await run(LaneChecks.refusal(guarded(null, []), hosted(undefined), {}))).toBe(
      "lab-1 has no harness on its SUT's routing path (backend.mode = none in /lanes/lab-1.toml): " +
        "no cell can run against it here, forced or not"
    )
  })
})

describe("the batch-spec", () => {
  it("states the lane, the pack's wiring, the scratch and the ledger where one is named", () => {
    expect(ReplayBatch.batchSpec(hosted("127.0.0.1:2"), { bl: "127.0.0.1:2", extra: 1 }, "/w", "/ledger")).toEqual({
      lane: "lab-1",
      bl: "127.0.0.1:2",
      extra: 1,
      work_dir: "/w",
      ledger_dir: "/ledger"
    })
    expect(ReplayBatch.batchSpec(hosted("127.0.0.1:2"), {}, "/w", undefined)).toEqual({ lane: "lab-1", work_dir: "/w" })
  })
})

describe("a named campaign's setup", () => {
  const rig = { operations: new Map([["seed", {}]]) } as unknown as Parameters<typeof RigWiring.setupRefusalOf>[0]

  it("names only operations the path defines, each once", () => {
    expect(RigWiring.setupRefusalOf(rig, "tool.toml", { campaign: { nightly: { setup: ["seed"] } } } as never)).toBeNull()
    expect(RigWiring.setupRefusalOf(rig, "tool.toml", { campaign: { nightly: { setup: ["wipe"] } } } as never)).toBe(
      "tool.toml: campaign.nightly.setup: names wipe, which no operation of the path defines"
    )
    expect(RigWiring.setupRefusalOf(rig, "tool.toml", { campaign: { nightly: { setup: ["seed", "seed"] } } } as never))
      .toBe("tool.toml: campaign.nightly.setup: names seed twice")
  })
})
