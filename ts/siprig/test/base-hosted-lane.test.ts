/**
 * The base's hosted lane is refused before it is driven where the base
 * cannot host it: a lane serving a backend its B2BUA runner never asks, a
 * lane naming no `b2bua-runner` part, a part lacking an address the base
 * stands the runner with, a `sip-capture` part lacking its file or named by
 * a standing lane, whose runner the base does not start. A lane stating all
 * of it is hosted, its runner standalone at the lane's ingress, capturing to
 * the `sip-capture` part's file where the lane names one.
 */
import type { LaneView } from "@sip/rig"
import { describe, expect, it } from "vitest"
import { CAPTURE_PART, hostedRefusalOf, RUNNER_PART, runnerEnvOf } from "../src/base-hosted-lane.ts"

const PART = { metrics: "127.0.0.1:9161", admin: "127.0.0.1:9162" }
const CAPTURE = { file: "/var/tmp/reference.pcapng" }

/** What a lane names beside its backend mode: its runner part, its capture part, its lifecycle. */
interface Named {
  readonly part?: Record<string, unknown>
  readonly capture?: Record<string, unknown>
  readonly lifecycle?: "managed" | "standing"
}

/** A hosted lane serving `mode`, naming the parts `named` states. */
const laneOf = (mode: string, named: Named = {}): LaneView.Hosted => {
  const parts: Array<[string, unknown]> = []
  if (named.part !== undefined) {
    parts.push([RUNNER_PART, { kind: RUNNER_PART, name: "lab", file: "targets/b2bua-runner/lab.toml", value: named.part }])
  }
  if (named.capture !== undefined) {
    parts.push([CAPTURE_PART, { kind: CAPTURE_PART, name: "lab", file: "targets/sip-capture/lab.toml", value: named.capture }])
  }
  return {
    kind: "hosted",
    id: "reference",
    backendMode: mode,
    sip: { lifecycle: named.lifecycle ?? "managed", ingress: "127.0.0.1:5160" },
    loaded: { parts: new Map(parts) }
  } as unknown as LaneView.Hosted
}

describe("the base's hosted lane", () => {
  it("is refused where it serves a backend, naming the mode it must state", () => {
    expect(hostedRefusalOf(laneOf("served", { part: PART }))).toContain(`state [backend] mode = "none"`)
  })

  it("is refused where it names no runner part, or a part lacking an address", () => {
    expect(hostedRefusalOf(laneOf("none"))).toContain(`names a ${RUNNER_PART} part`)
    expect(hostedRefusalOf(laneOf("none", { part: { metrics: PART.metrics } }))).toMatch(/targets\/b2bua-runner\/lab\.toml: .*admin/s)
  })

  it("is hosted where it states both, its runner standalone at the lane's ingress", () => {
    const lane = laneOf("none", { part: PART })
    expect(hostedRefusalOf(lane)).toBeNull()
    expect(runnerEnvOf(lane, PART)).toEqual({
      B2BUA_MODE: "standalone",
      B2BUA_LISTEN: "127.0.0.1:5160",
      B2BUA_ADVERTISE: "127.0.0.1:5160",
      B2BUA_METRICS: PART.metrics,
      B2BUA_ADMIN: PART.admin
    })
  })

  it("stands a runner capturing its SIP datagrams to the file the lane's sip-capture part names", () => {
    const lane = laneOf("none", { part: PART, capture: CAPTURE })
    expect(hostedRefusalOf(lane)).toBeNull()
    expect(runnerEnvOf(lane, PART)["B2BUA_CAPTURE"]).toBe(CAPTURE.file)
  })

  it("is refused where its sip-capture part names no file, or where the lane is standing", () => {
    expect(hostedRefusalOf(laneOf("none", { part: PART, capture: {} }))).toMatch(/targets\/sip-capture\/lab\.toml: .*file/s)
    expect(hostedRefusalOf(laneOf("none", { part: PART, capture: CAPTURE, lifecycle: "standing" }))).toContain(
      `the base captures only a runner it stands: a standing lane names no ${CAPTURE_PART} part`
    )
  })
})
