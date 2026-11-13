/**
 * The base's hosted lane is refused before it is driven where the base
 * cannot host it: a lane serving a backend its B2BUA runner never asks, a
 * lane naming no `b2bua-runner` part, a part lacking an address the base
 * stands the runner with. A lane stating all of it is hosted, its runner
 * standalone at the lane's ingress.
 */
import type { LaneView } from "@sip/rig"
import { describe, expect, it } from "vitest"
import { hostedRefusalOf, RUNNER_PART, runnerEnvOf } from "../src/base-hosted-lane.ts"

const PART = { metrics: "127.0.0.1:9161", admin: "127.0.0.1:9162" }

/** A hosted lane serving `mode`, naming `part` as its runner's where given. */
const laneOf = (mode: string, part?: Record<string, unknown>): LaneView.Hosted =>
  ({
    kind: "hosted",
    id: "reference",
    backendMode: mode,
    sip: { lifecycle: "managed", ingress: "127.0.0.1:5160" },
    loaded: {
      parts: new Map(part === undefined ? [] : [[RUNNER_PART, { kind: RUNNER_PART, name: "lab", file: "targets/b2bua-runner/lab.toml", value: part }]])
    }
  }) as unknown as LaneView.Hosted

describe("the base's hosted lane", () => {
  it("is refused where it serves a backend, naming the mode it must state", () => {
    expect(hostedRefusalOf(laneOf("served", PART))).toContain(`state [backend] mode = "none"`)
  })

  it("is refused where it names no runner part, or a part lacking an address", () => {
    expect(hostedRefusalOf(laneOf("none"))).toContain(`names a ${RUNNER_PART} part`)
    expect(hostedRefusalOf(laneOf("none", { metrics: PART.metrics }))).toMatch(/targets\/b2bua-runner\/lab\.toml: .*admin/s)
  })

  it("is hosted where it states both, its runner standalone at the lane's ingress", () => {
    const lane = laneOf("none", PART)
    expect(hostedRefusalOf(lane)).toBeNull()
    expect(runnerEnvOf(lane, PART)).toEqual({
      B2BUA_MODE: "standalone",
      B2BUA_LISTEN: "127.0.0.1:5160",
      B2BUA_ADVERTISE: "127.0.0.1:5160",
      B2BUA_METRICS: PART.metrics,
      B2BUA_ADMIN: PART.admin
    })
  })
})
