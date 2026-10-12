/**
 * REFER with `Replaces` is out of the format's scope: the group carrying it is
 * refused whole, the datagram named, and nothing else that mentions the word
 * is charged.
 */
import type { Flows } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import type { CaptureInput } from "../src/capture-rules.js"
import { REFER_REPLACES, REFER_REPLACES_RULE } from "../src/refer-replaces.js"
import { doc, leg, oneHop, request, response, SOCKETS, sutSet } from "./fixtures.js"

const TRANSFEROR_CALL_ID = "transferor-leg-call-id"
const CONSULTATION_CALL_ID = "consultation-leg-call-id"

type TransferShape =
  | "refer-replaces-header"
  | "refer-to-embedded-replaces"
  | "invite-replaces-header"
  | "blind"
  | "option-tag-only"

const SUPPORTED = "Supported: timer,replaces,info"
const REPLACES = `Replaces: ${CONSULTATION_CALL_ID};to-tag=callee-tag;from-tag=from-${CONSULTATION_CALL_ID}`

const referHeaders = (shape: TransferShape): ReadonlyArray<string> => {
  switch (shape) {
    case "refer-replaces-header":
      return [SUPPORTED, `Refer-To: <sip:transfer-target@example.invalid>`, REPLACES]
    case "refer-to-embedded-replaces":
      return [
        SUPPORTED,
        `Refer-To: <sip:transfer-target@example.invalid?Replaces=${CONSULTATION_CALL_ID}` +
        `%3Bto-tag%3Dcallee-tag%3Bfrom-tag%3Dfrom-${CONSULTATION_CALL_ID}>`
      ]
    default:
      return [SUPPORTED, `Refer-To: <sip:transfer-target@example.invalid>`]
  }
}

/**
 * An answered call whose caller then transfers it: INVITE / 200 / ACK, the REFER
 * and its 202, then the teardown. `option-tag-only` sends no REFER at all, and
 * `invite-replaces-header` puts the header on the initial INVITE — the transfer
 * target's own call, captured without the REFER that caused it.
 */
const transferFlows = (shape: TransferShape): Flows.FlowsDoc => {
  const { caller, sut } = SOCKETS
  const inviteHeaders = shape === "invite-replaces-header" ? [SUPPORTED, REPLACES] : [SUPPORTED]
  const id = TRANSFEROR_CALL_ID
  const msgs: Array<Flows.Msg> = [
    request({ callId: id, seq: 1, method: "INVITE", src: caller, dst: sut, ts_ms: 0, headers: inviteHeaders }),
    response({ callId: id, seq: 1, status: 200, reason: "OK", cseqMethod: "INVITE", src: sut, dst: caller, ts_ms: 200, toTag: "sut-tag" }),
    request({ callId: id, seq: 1, method: "ACK", src: caller, dst: sut, ts_ms: 205, toTag: "sut-tag", headers: [SUPPORTED] })
  ]
  if (shape !== "option-tag-only") {
    msgs.push(
      request({ callId: id, seq: 2, method: "REFER", src: caller, dst: sut, ts_ms: 900, toTag: "sut-tag", headers: referHeaders(shape) }),
      response({ callId: id, seq: 2, status: 202, reason: "Accepted", cseqMethod: "REFER", src: sut, dst: caller, ts_ms: 910, toTag: "sut-tag" })
    )
  }
  msgs.push(
    request({ callId: id, seq: 3, method: "BYE", src: caller, dst: sut, ts_ms: 1_500, toTag: "sut-tag", headers: [SUPPORTED] }),
    response({ callId: id, seq: 3, status: 200, reason: "OK", cseqMethod: "BYE", src: sut, dst: caller, ts_ms: 1_505, toTag: "sut-tag" })
  )
  return doc([leg(id, oneHop(caller, sut), msgs)], [{ legs: [0] }])
}

const inputOf = (flows: Flows.FlowsDoc, legs: ReadonlyArray<number> = [0]): CaptureInput => ({
  flows,
  capture: "synthetic.pcap.gz",
  sut: sutSet(),
  legs,
  callIds: [TRANSFEROR_CALL_ID],
  caseId: "case-a"
})

const findingOf = (flows: Flows.FlowsDoc, legs?: ReadonlyArray<number>) =>
  REFER_REPLACES_RULE.refuses(inputOf(flows, legs))

describe("REFER with Replaces", () => {
  it("refuses a REFER carrying a Replaces header, naming the datagram", () => {
    const finding = findingOf(transferFlows("refer-replaces-header"))!
    expect(finding.groups).toEqual([0])
    expect(finding.evidence).toEqual([{ group: 0, leg: 0, msg: 3, detail: "REFER carries a Replaces header" }])
    expect(finding.line).toBe(
      "synthetic.pcap.gz: case 'case-a' EXCLUDED refer-replaces-out-of-scope — " +
        "group 0 leg 0 msg 3 (REFER carries a Replaces header)"
    )
  })

  it("refuses a Refer-To whose embedded URI carries Replaces=", () => {
    expect(findingOf(transferFlows("refer-to-embedded-replaces"))!.evidence?.map((e) => e.detail)).toEqual([
      "REFER's Refer-To embeds a Replaces= parameter"
    ])
  })

  /** The transfer target's own call, captured without the REFER that caused it. */
  it("refuses an INVITE that names another dialog", () => {
    expect(findingOf(transferFlows("invite-replaces-header"))!.evidence).toEqual([
      { group: 0, leg: 0, msg: 0, detail: "INVITE carries a Replaces header" }
    ])
  })

  it("leaves a blind REFER alone", () => {
    expect(findingOf(transferFlows("blind"))).toBeUndefined()
  })

  it("never fires on a Supported: replaces option tag", () => {
    const flows = transferFlows("option-tag-only")
    const requests = flows.legs[0]!.msgs.filter((m) => m.summary.kind === "request")
    expect(requests.every((m) => ("raw" in m ? m.raw : "").includes(SUPPORTED))).toBe(true)
    expect(findingOf(flows)).toBeUndefined()
  })

  it("reads only the groups the case was cut from", () => {
    expect(findingOf(transferFlows("refer-replaces-header"), [7])).toBeUndefined()
  })

  it("refuses outright, under the format's scope", () => {
    expect([REFER_REPLACES_RULE.id, REFER_REPLACES_RULE.subject, REFER_REPLACES_RULE.disposition]).toEqual([
      REFER_REPLACES,
      "scope",
      "refuses"
    ])
  })
})
