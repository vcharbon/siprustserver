/**
 * A PRACK no captured reliable provisional asks for: the document cannot state
 * the exchange (RFC 3262 §4: a PRACK acknowledges one reliable provisional),
 * so it is refused under its own source token, and a PRACK answering a
 * reliable provisional the leg received is left alone.
 */
import { Tokens, type Flow } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { DOCUMENT_RULES } from "../src/document-rules.js"
import { PRACK_UNANSWERABLE, unansweredPracks } from "../src/unanswered-prack.js"

const DELAY: Flow.Delay = {
  ms: 0,
  from: Tokens.anchorToken({ _tag: "trigger" }),
  compressible: true,
  timer_linked: false
}

const step = (id: string, leg: string, op: "send" | "expect", msg: Flow.Step["msg"]): Flow.Step => ({
  id,
  leg,
  op,
  msg,
  delay: DELAY
})

const invite = (id: string, leg: string, op: "send" | "expect" = "send") => step(id, leg, op, { method: "INVITE" })
const final = (id: string, leg: string, status: number, op: "send" | "expect" = "send") =>
  step(id, leg, op, { status, "cseq-method": "INVITE" })
const prack = (id: string, leg: string, op: "send" | "expect") => step(id, leg, op, { method: "PRACK" })

/**
 * The caller PRACKs a reliable 18x the capture does not hold, and the system
 * under test passes the PRACK on to a callee leg that holds none either.
 * Replayed, the caller's PRACK has no provisional to RAck and its send fails.
 */
const unanswerable: ReadonlyArray<Flow.FlowNode> = [
  invite("s1", "A"),
  step("s2", "A", "expect", { status: 100, "cseq-method": "INVITE" }),
  invite("s3", "B", "expect"),
  step("s4", "B", "send", { status: 100, "cseq-method": "INVITE" }),
  prack("s5", "A", "send"),
  prack("s6", "B", "expect"),
  step("s7", "B", "send", { status: 200, "cseq-method": "PRACK" }),
  step("s8", "A", "expect", { status: 200, "cseq-method": "PRACK" }),
  final("s9", "B", 200),
  final("s10", "A", 200, "expect")
]

const refusedBy = (flow: ReadonlyArray<Flow.FlowNode>) =>
  DOCUMENT_RULES.flatMap((rule) => {
    const finding = rule.refuses({ capture: "c.pcap", caseId: "c.pcap-case1", flow, calls: [], declared: [] })
    return finding === undefined ? [] : [{ id: rule.id, line: finding.line }]
  })

describe("a PRACK no captured reliable provisional asks for", () => {
  it("is refused by the pipeline's own document rule, naming the step", () => {
    expect(refusedBy(unanswerable)).toEqual([{
      id: PRACK_UNANSWERABLE,
      line: "c.pcap: case 'c.pcap-case1' EXCLUDED source-prack-unanswerable — leg A step s5 sends a PRACK " +
        "and the leg received no reliable provisional before it"
    }])
  })

  it("leaves a PRACK answering a reliable provisional the leg received", () => {
    const reliable = step("s2", "A", "expect", {
      status: 183,
      "cseq-method": "INVITE",
      headers: [{ name: "RSeq", value: "1" }]
    })
    expect(unansweredPracks([invite("s1", "A"), reliable, prack("s3", "A", "send")])).toEqual([])
  })

  it("charges the PRACK, never the 100 Trying, which is no reliable provisional", () => {
    expect(unansweredPracks(unanswerable).map((charged) => charged.step)).toEqual(["s5"])
  })
})
