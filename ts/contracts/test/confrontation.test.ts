import { describe, expect, it } from "vitest"
import { Confrontation } from "../src/index.js"

const record: Confrontation.ConfrontationRecord = {
  lane: "fake",
  capture: "capture_x.pcap.gz",
  case: "capture_x.pcap.gz-case1",
  run: 0,
  step: "s4",
  kind: "header",
  signature: "header:contact:response:200:INVITE",
  name: "Contact",
  scope: "response:200:INVITE",
  captured: ["<sip:a@1.2.3.4>"],
  replayed: ["<sip:b@127.0.0.1:5061>"],
  inbound: false,
  added: [],
  removed: [],
  class: "accepted",
  rule: "contact-host-is-replay-socket",
  ticket: ""
}

describe("the confrontation record", () => {
  it("round-trips through its NDJSON line", () => {
    const line = Confrontation.emitConfrontationRecord(record)
    expect(line.includes("\n")).toBe(false)
    const back = Confrontation.parseConfrontationLines(`${line}\n`)
    expect(back).toEqual([record])
  })

  it("every key is present on the emitted line — the jq contract", () => {
    const line = JSON.parse(Confrontation.emitConfrontationRecord(record)) as Record<string, unknown>
    for (
      const key of [
        "lane",
        "capture",
        "case",
        "run",
        "step",
        "kind",
        "signature",
        "name",
        "scope",
        "captured",
        "replayed",
        "inbound",
        "added",
        "removed",
        "class",
        "rule",
        "ticket"
      ]
    ) {
      expect(line, key).toHaveProperty(key)
    }
  })

  it("refuses an unknown key rather than ignoring it", () => {
    expect(() =>
      Confrontation.decodeConfrontationRecordSync({ ...record, extra: true } as unknown)
    ).toThrow()
  })

  it("refuses a class outside the vocabulary", () => {
    expect(() =>
      Confrontation.decodeConfrontationRecordSync({ ...record, class: "blessed" } as unknown)
    ).toThrow()
  })

  it("passes exactly when nothing is unlisted or unknown", () => {
    const unknown = { ...record, class: "unknown" as const }
    const knownBug = { ...record, class: "known-bug" as const }
    expect(Confrontation.recordsPass([record, knownBug])).toBe(true)
    expect(Confrontation.recordsPass([record, unknown])).toBe(false)
    expect(Confrontation.recordsPass([])).toBe(true)
  })
})

describe("the classification summary", () => {
  const summary: Confrontation.ClassificationSummary = {
    case: "capture_x.pcap.gz-case1",
    lane: "fake",
    mode: "record",
    records: 3,
    accepted: 2,
    known_bug: 1,
    unlisted: 0,
    unknown: 0,
    compared: 12,
    unreferenced: 1,
    passed: true
  }

  it("round-trips canonically", () => {
    const text = Confrontation.emitClassificationSummary(summary)
    expect(Confrontation.decodeClassificationSummarySync(JSON.parse(text) as unknown)).toEqual(
      summary
    )
  })

  it("refuses a mode outside record/enforce", () => {
    expect(() =>
      Confrontation.decodeClassificationSummarySync({ ...summary, mode: "strict" } as unknown)
    ).toThrow()
  })
})
