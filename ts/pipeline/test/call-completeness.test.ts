/**
 * Source-call completeness: a case whose caller's dialog the capture never
 * finished — answered and never torn down, or offered and never finalled — is
 * refused; a failed attempt is complete, and a dialog the system under test
 * dialled out itself is exempt.
 */
import { describe, expect, it } from "vitest"
import { CALL_COMPLETENESS_RULE, SOURCE_CALL_INCOMPLETE } from "../src/call-completeness.js"
import { CASE_RULES } from "../src/case-rules.js"
import { cutCalls, type CutVantage, vantageIsInbound } from "../src/cut.js"
import { SutSet } from "../src/sut.js"
import { BASE_CALL_ID, completenessFlows, SUT_IP } from "./cutfixtures.js"
import { derivesOnePrefix, nameByCallIds } from "./fixtures.js"

const CAPTURE = "synthetic-completeness.pcap.gz"
const sut = new SutSet([SUT_IP])

type Flows = ReturnType<typeof completenessFlows>

/** The cut's own vantages, so a test judges what the generator would judge. */
const cutOf = (flows: Flows): ReadonlyArray<CutVantage> => {
  const call = cutCalls(flows, sut, CAPTURE, derivesOnePrefix, nameByCallIds).calls[0]!
  return [call.uac, ...call.uas]
}

const gate = (flows: Flows, vantages: ReadonlyArray<CutVantage> = cutOf(flows)) =>
  CALL_COMPLETENESS_RULE.refuses({
    flows,
    capture: CAPTURE,
    sut,
    legs: vantages.map((v) => v.leg),
    callIds: [],
    caseId: "case1",
    spec: { id: "case1", uac: vantages[0]!, uas: vantages.slice(1), defects: [], cutLegs: vantages.map((v) => v.leg) }
  })

describe("source-call completeness", () => {
  it("refuses a caller dialog the capture answered and never tore down", () => {
    const finding = gate(completenessFlows({ caller: "left-open", derived: "torn-down" }))!
    expect(finding.legs).toEqual([0])
    expect(finding.callIds).toEqual([BASE_CALL_ID])
    expect(finding.evidence).toEqual([{
      leg: 0,
      hop: 0,
      callId: BASE_CALL_ID,
      msg: 1,
      detail: "took the 200 answering its INVITE (msg 1) and no BYE ever closed the dialog in the capture"
    }])
    expect(finding.line).toBe(
      `${CAPTURE}: case 'case1' EXCLUDED source-call-incomplete — caller leg 0 hop 0 on ` +
        `'${BASE_CALL_ID}' took the 200 answering its INVITE (msg 1) and no BYE ever closed ` +
        `the dialog in the capture`
    )
  })

  /** The capture stopped inside the INVITE transaction — a 100 and nothing else. */
  it("refuses a caller INVITE the capture holds no final for", () => {
    const finding = gate(completenessFlows({ caller: "unanswered", derived: "unanswered" }))!
    expect(finding.evidence).toEqual([{
      leg: 0,
      hop: 0,
      callId: BASE_CALL_ID,
      msg: 0,
      detail: "took the INVITE opening the dialog (msg 0) and no final response to it was ever captured"
    }])
  })

  it("passes a caller dialog a BYE closed", () => {
    expect(gate(completenessFlows({ caller: "torn-down", derived: "torn-down" }))).toBeUndefined()
  })

  /** A FAILED attempt is complete: it reached its own end, and nothing is left up to tear down. */
  it("passes a failed attempt — an error final, or a CANCELled INVITE", () => {
    expect(gate(completenessFlows({ caller: "rejected", derived: "rejected" }))).toBeUndefined()
    expect(gate(completenessFlows({ caller: "cancelled", derived: "cancelled" }))).toBeUndefined()
  })

  /** RFC 3261 §9.1 gives a CANCEL no effect once the 2xx is out, and §15 leaves BYE as the only end. */
  it("refuses a CANCEL that lost the race to the 2xx", () => {
    expect(gate(completenessFlows({ caller: "cancelled-but-answered", derived: "torn-down" }))).toBeDefined()
  })

  it("exempts a dialog the system under test dialled out and the capture left open", () => {
    const flows = completenessFlows({ caller: "torn-down", derived: "left-open" })
    const vantages = cutOf(flows)
    expect(vantages).toHaveLength(2)
    expect(vantageIsInbound(flows, sut, vantages[1]!)).toBe(false)
    expect(gate(flows, vantages)).toBeUndefined()
  })

  it("judges every caller-side ingress vantage, not only the cut's uac", () => {
    const flows = completenessFlows({ caller: "torn-down", derived: "left-open", derivedArrives: true })
    const vantages = cutCalls(flows, sut, CAPTURE, derivesOnePrefix, nameByCallIds).calls.map((c) => c.uac)
    expect(vantages.map((v) => vantageIsInbound(flows, sut, v))).toEqual([true, true])
    expect(gate(flows, vantages)!.legs).toEqual([1])
  })

  /** The fixtures fix every leg's summary at `200` / no terminator, whatever the wire holds. */
  it("reads the datagrams, never the leg's own summary", () => {
    const closed = completenessFlows({ caller: "torn-down", derived: "torn-down" })
    expect(closed.legs[0]!.terminated_by).toBeNull()
    expect(gate(closed)).toBeUndefined()
    const rejected = completenessFlows({ caller: "rejected", derived: "rejected" })
    expect(rejected.legs[0]!.final_status).toBe(200)
    expect(gate(rejected)).toBeUndefined()
  })

  it("does not re-open a closed dialog with a 2xx retransmitted after the BYE", () => {
    expect(gate(completenessFlows({ caller: "retransmitted-after-bye", derived: "torn-down" }))).toBeUndefined()
  })

  /** The BYE has to close the dialog that was ANSWERED LAST. */
  it("refuses a dialog answered after the previous one was torn down", () => {
    const flows = completenessFlows({ caller: "re-answered-after-teardown", derived: "torn-down" })
    expect(gate(flows)!.evidence?.[0]!.msg).toBe(6)
  })

  it("names nothing where the case holds no caller-side vantage at all", () => {
    expect(gate(completenessFlows({ caller: "left-open", derived: "left-open" }), [{ leg: 1, hop: 0 }])).toBeUndefined()
  })

  /** A vantage is cut from the document every rule reads; one naming no leg of it is a defect, never a pass. */
  it("throws on a vantage naming no leg of the document rather than passing the case", () => {
    expect(() => gate(completenessFlows({ caller: "torn-down", derived: "torn-down" }), [{ leg: 9, hop: 0 }])).toThrow(
      /vantage leg 9 hop 0 names no leg of the flows document/
    )
  })

  it("is the pipeline's case-tier rule, after the re-offer rule", () => {
    expect(CASE_RULES.map((r) => r.id)).toEqual([
      "source-capture-misordered",
      "source-truncated-at-edge",
      "scope-identity-reoffered",
      SOURCE_CALL_INCOMPLETE
    ])
  })
})
