/**
 * The case-document tier's own decision: which of the charges its rules compute
 * survive the declarations the document already carries.
 */
import { Tokens, type Flow, type MustFail } from "@sip/contracts"
import { describe, expect, it } from "vitest"
import { DOCUMENT_RULES, type DocumentInput } from "../src/document-rules.js"

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

/** A callee answering the SUT's INVITE with no ACK captured, the ACK relayed on B. */
const unackedOnA: ReadonlyArray<Flow.FlowNode> = [
  step("s1", "A", "expect", { method: "INVITE" }),
  step("s2", "A", "send", { status: 200, "cseq-method": "INVITE" }),
  step("s3", "B", "send", { method: "ACK" })
]

const refusalOf = (id: string, input: DocumentInput): string | undefined =>
  DOCUMENT_RULES.find((rule) => rule.id === id)!.refuses(input)?.line

const input = (declared: ReadonlyArray<MustFail.MustFail>): DocumentInput => ({
  capture: "capture_x.pcap.gz",
  caseId: "capture_x.pcap.gz-auto",
  flow: unackedOnA,
  declared
})

describe("a hole the declaration lane already names", () => {
  it("is refused where nothing declares it", () => {
    expect(refusalOf("source-ack-not-captured", input([]))).toContain("captured no ACK for it")
  })

  it("is not refused where the document owes that failure at that step", () => {
    expect(
      refusalOf(
        "source-ack-not-captured",
        input([
          { failure: "unexpected-ack", step: "s2", derived_from: "no-ack-to-dialog-creating-2xx" }
        ])
      )
    ).toBeUndefined()
  })

  it("is still refused where the declaration names a DIFFERENT step", () => {
    expect(
      refusalOf(
        "source-ack-not-captured",
        input([
          { failure: "unexpected-ack", step: "s9", derived_from: "no-ack-to-dialog-creating-2xx" }
        ])
      )
    ).toContain("captured no ACK for it")
  })

  it("is still refused where the declaration names a different FAILURE", () => {
    expect(
      refusalOf(
        "source-ack-not-captured",
        input([
          { failure: "unexpected-prack", step: "s2", derived_from: "unacked-reliable-provisional" }
        ])
      )
    ).toContain("captured no ACK for it")
  })
})
