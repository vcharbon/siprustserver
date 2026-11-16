/**
 * The facts the RFC rows read: each PRACK the system sent pairs with the RSeq
 * the leg's actor sent on the reliable provisional it acknowledges.
 */
import { documentReadingOf } from "@sip/delta-rules/document-reading"
import type { Classifier } from "@sip/pipeline"
import { describe, expect, it } from "vitest"
import { acknowledgedRSeqAtStep, rfcFactsOf } from "../src/facts.js"

const step = (id: string, leg: string, op: "send" | "expect", over: Partial<Classifier.DocumentStep>): Classifier.DocumentStep =>
  ({ id, leg, op, auto: false, optional: false, inDialog: false, ...over })

const documentOf = (steps: ReadonlyArray<Classifier.DocumentStep>): Classifier.DocumentShape =>
  ({ unackedFinals: [], calls: 1, steps, attempts: [] })

const runSending = (rseqs: ReadonlyArray<readonly [string, ReadonlyArray<number>]>): Classifier.RunShape => ({
  messages: 10,
  legs: 2,
  legsTornDownBySystem: 0,
  sentRSeqs: new Map(rseqs),
  openers: new Map(),
  receptions: new Map(),
  sent: new Map()
})

describe("the RSeq a PRACK acknowledges (RFC 3262 §7.2)", () => {
  it("pairs each of the system's PRACKs with the actor's RSeq of the reliable provisional before it, by position", () => {
    const steps = [
      step("s5", "B", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s7", "B", "expect", { method: "PRACK" }),
      step("s8", "B", "send", { status: 180, cseqMethod: "INVITE" }),
      step("s9", "B", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s10", "B", "expect", { method: "PRACK" }),
      step("s11", "A", "expect", { method: "PRACK" })
    ]
    const read = acknowledgedRSeqAtStep(documentReadingOf(documentOf(steps)), runSending([["B", [4711, 4712]]]))
    expect([...read]).toEqual([["s7", 4711], ["s10", 4712]])
  })

  it("counts a reliable provisional, not the INVITE that asked for reliability", () => {
    const steps = [
      step("s1", "A", "send", { method: "INVITE", reliable: true, sdp: true }),
      step("s2", "A", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s3", "A", "expect", { method: "PRACK", inDialog: true })
    ]
    const read = acknowledgedRSeqAtStep(documentReadingOf(documentOf(steps)), runSending([["A", [7, 8]]]))
    expect([...read]).toEqual([["s3", 7]])
  })

  it("is the fact a deployment's case facts take whole from rfcFactsOf", () => {
    const steps = [
      step("s2", "A", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s3", "A", "expect", { method: "PRACK", inDialog: true })
    ]
    const facts = rfcFactsOf(documentReadingOf(documentOf(steps)), runSending([["A", [7]]]))
    expect(Object.keys(facts)).toEqual(["acknowledgedRSeqAtStep"])
    expect([...facts.acknowledgedRSeqAtStep]).toEqual([["s3", 7]])
  })

  it("leaves out a PRACK whose provisional the run never sent", () => {
    const steps = [
      step("s2", "A", "send", { status: 183, cseqMethod: "INVITE", reliable: true }),
      step("s3", "A", "expect", { method: "PRACK", inDialog: true })
    ]
    const read = acknowledgedRSeqAtStep(documentReadingOf(documentOf(steps)), runSending([]))
    expect([...read]).toEqual([])
  })
})
