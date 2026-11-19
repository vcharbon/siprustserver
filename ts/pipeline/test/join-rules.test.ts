/**
 * The join rules: a leg the platform dialled after it accepted an in-dialog
 * request on another leg is joined on that request, and a dialog carrying a
 * media-server control body is a resource; a leg both rules claim is the later
 * rule's.
 */
import { describe, expect, it } from "vitest"
import { joinsOf, type JoinRule } from "../src/join-rules.js"
import { scenario, type Scenario, type ScenarioLeg } from "../src/scenario.js"
import type * as Topology from "../src/topology.js"
import { actorOf } from "./scenario-actors.js"

const CALLEE = "198.51.100.2:5060"
const TARGET = "198.51.100.3:5060"
const TRANSFER = "application/vnd.example.transfer"
const CONTROL = "application/mediaservercontrol+xml"

const REFER: JoinRule = { on: "accepted-request", kind: "refer", method: "REFER", label: "REFER" }
const INFO: JoinRule = { on: "accepted-request", kind: "info", method: "INFO", contentType: TRANSFER, label: "transfer INFO" }
const RESOURCE: JoinRule = { on: "control-body", kind: "mrf", contentTypes: [CONTROL], releaseWindowMs: 500 }

/** The callee: answers, then sends `method` (with a body of `type`), which the platform answers `status`. */
const askingLeg = (s: Scenario, method: string, status: number, type?: string): ScenarioLeg =>
  s
    .leg({ callId: "b@example.com", party: CALLEE })
    .invite({ at: 10, by: "system" })
    .response(200, { at: 230 })
    .ack({ at: 240, by: "system" })
    .request(method, { at: 30_000, by: "party", ...(type === undefined ? {} : { body: { type, text: "x\r\n" } }) })
    .response(status, { at: 30_010 })

/** A leg the platform dials at `at` ms. */
const dialled = (s: Scenario, at: number): ScenarioLeg =>
  s.leg({ callId: "c@example.com", party: TARGET }).invite({ at, by: "system" }).response(200, { at: at + 900 })

const read = (s: Scenario, legs: ReadonlyArray<ScenarioLeg>, rules: ReadonlyArray<JoinRule>): Topology.JoinsReading => {
  const flows = s.flows()
  return joinsOf(rules)(flows, legs.map((leg) => actorOf(s, flows, leg, { kind: "uas", pivotLeg: String.fromCharCode(66 + leg.index) })))
}

describe("an accepted-request join rule", () => {
  it("joins the leg dialled after the accepted request, naming it by the rule's label", () => {
    const s = scenario()
    const joined = read(s, [askingLeg(s, "REFER", 202), dialled(s, 30_100)], [REFER])
    expect([...joined.keys()]).toEqual([1])
    expect(joined.get(1)!.kind).toBe("refer")
    expect(joined.get(1)!.evidence[0]).toBe(
      "dialled 90 ms after the platform accepted leg B's REFER — a transferee, not a hunted callee"
    )
  })

  it("joins nothing off a refused request, a leg dialled before it, or another body type", () => {
    const refused = scenario()
    expect(read(refused, [askingLeg(refused, "REFER", 403), dialled(refused, 30_100)], [REFER]).size).toBe(0)
    const early = scenario()
    expect(read(early, [askingLeg(early, "REFER", 202), dialled(early, 20)], [REFER]).size).toBe(0)
    const other = scenario()
    expect(read(other, [askingLeg(other, "INFO", 200, "application/vnd.example.other"), dialled(other, 30_100)], [INFO]).size).toBe(0)
  })

  it("joins on an INFO only where it carries the rule's body type", () => {
    const s = scenario()
    const joined = read(s, [askingLeg(s, "INFO", 200, TRANSFER), dialled(s, 30_100)], [INFO])
    expect(joined.get(1)!.evidence[0]).toContain("accepted leg B's transfer INFO")
  })
})

describe("a control-body join rule", () => {
  it("reads a dialog carrying the control body as a resource, the protocol typed", () => {
    const s = scenario()
    const resource = s
      .leg({ callId: "m@example.com", party: TARGET })
      .invite({ at: 10, by: "system" })
      .response(200, { at: 15 })
      .ack({ at: 16, by: "system" })
      .request("INFO", { at: 100, by: "system", body: { type: CONTROL, text: "<x/>\r\n" } })
      .response(200, { at: 101 })
    const joined = read(s, [resource], [RESOURCE])
    expect(joined.get(0)).toMatchObject({ kind: "mrf", protocol: CONTROL })
    expect(joined.get(0)!.evidence).toEqual([
      `media-server control dialog: 1 ${CONTROL} body(ies) on this leg — no hunted callee carries one`,
      "the resource answered 5 ms after its own INVITE",
      "the capture shows no SUT-originated BYE on this leg"
    ])
  })

  it("claims a leg an earlier rule joined", () => {
    const s = scenario()
    const target = dialled(s, 30_100).request("INFO", { at: 31_100, by: "system", body: { type: CONTROL, text: "<x/>\r\n" } })
    const joined = read(s, [askingLeg(s, "REFER", 202), target], [REFER, RESOURCE])
    expect(joined.get(1)!.kind).toBe("mrf")
  })
})
