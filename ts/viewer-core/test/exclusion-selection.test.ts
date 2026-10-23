/**
 * A refused case drawn from its record: the legs and datagrams the record
 * names are the selection, the cut's SUT orients the ladder, and the text
 * export prints the refusal where a verdict would stand.
 */
import { describe, expect, it } from "vitest"
import { exclusionSelection, exclusionSut } from "../src/exclusion/selection.ts"
import { parseExclusions } from "../src/exclusion/parse.ts"
import { buildFlowsLayout } from "../src/flows/layout.ts"
import { parseFlowsDocument } from "../src/flows/parse.ts"
import { exclusionsToText } from "../src/text/exclusion-text.ts"

const message = (tsUs: number, src: string, dst: string, summary: Record<string, unknown>) => ({
  ts_us: tsUs,
  src,
  dst,
  hop: 0,
  retx: false,
  raw: "",
  summary
})

const invite = (uri: string) => ({
  kind: "request",
  method: "INVITE",
  uri,
  cseq: { seq: 1, method: "INVITE" },
  from: { tag: "f" },
  to: {}
})

const ringing = { kind: "response", status: 180, reason: "Ringing", cseq: { seq: 1, method: "INVITE" }, from: { tag: "f" }, to: { tag: "t" } }

const PEER = "62.229.32.38:5060"
const SUT = "217.167.35.142:5061"
const FAR = "10.9.9.9:5060"

/** Three legs of one call through the SUT, and one leg of another call. */
const flows = {
  schema: 5,
  legs: [
    { call_id: "base", msgs: [message(0, PEER, SUT, invite("sip:a@x"))] },
    { call_id: "1-base", msgs: [message(10, SUT, FAR, invite("sip:b@x"))] },
    {
      call_id: "1000-base",
      msgs: [message(20, SUT, FAR, invite("sip:c@x")), message(30, FAR, SUT, ringing)]
    },
    { call_id: "other", msgs: [message(40, PEER, SUT, invite("sip:d@x"))] }
  ],
  groups: []
}

/** The record the generator writes for the case cut on legs 0, 1 and 2. */
const record = {
  caseId: "cap-case1",
  reason: "sut-18x-not-relayed",
  legs: [0, 1, 2],
  callIds: ["base", "1-base", "1000-base"],
  sut: ["217.167.35.142"],
  evidence: [
    {
      leg: 2,
      hop: 0,
      callId: "1000-base",
      msg: 1,
      detail: "took the 180 (msg 1) and sent no 18x of its own anywhere in the case"
    }
  ],
  line: "cap: case 'cap-case1' EXCLUDED sut-18x-not-relayed — leg 2 hop 0 on '1000-base' took the 180 (msg 1) and sent no 18x of its own anywhere in the case"
}

const records = () => {
  const read = parseExclusions([record])
  if (!read.ok) throw new Error(read.reason)
  return read.records
}

describe("a refused case's selection", () => {
  it("names the record's legs and the charged datagrams, and no step", () => {
    const selection = exclusionSelection(records())
    expect([...selection.legs].sort()).toEqual([0, 1, 2])
    expect([...selection.messages]).toEqual(["2/1"])
    expect(selection.stepByMessage.size).toBe(0)
    expect(selection.empty).toBe(false)
  })

  it("is empty for a record naming no leg, so the ladder draws the capture", () => {
    const read = parseExclusions([{ caseId: "c", reason: "source-file-too-short", line: "" }])
    if (!read.ok) throw new Error(read.reason)
    expect(exclusionSelection(read.records).empty).toBe(true)
    expect(exclusionSut(read.records)).toEqual([])
  })

  it("draws the refused call's legs alone, oriented on the cut's SUT", () => {
    const read = parseFlowsDocument(flows)
    if (!read.ok) throw new Error(read.reason)
    const layout = buildFlowsLayout(read.document, {
      selection: exclusionSelection(records()),
      sut: exclusionSut(records()),
      onlySelected: true,
      title: "cap"
    })
    expect(layout.rows.map((row) => row.legIndex)).toEqual([0, 1, 2, 2])
    expect(layout.sutSockets).toEqual([SUT])
    expect(layout.lifelines.find((line) => line.id === SUT)?.sut).toBe(true)
    expect(layout.subtitle).toContain("3 legs selected by this case")
    expect(layout.subtitle).toContain(`SUT at ${SUT}`)
  })

  it("orients on the steps' SUT first: the cut's address is the fallback", () => {
    const read = parseFlowsDocument(flows)
    if (!read.ok) throw new Error(read.reason)
    const selection = {
      ...exclusionSelection(records()),
      opByMessage: new Map([["0/0", "send" as const]])
    }
    const layout = buildFlowsLayout(read.document, { selection, sut: ["10.9.9.9"] })
    expect(layout.sutSockets).toEqual([SUT])
  })
})

describe("exclusionsToText", () => {
  it("prints the reason, the line and every charged datagram", () => {
    expect(exclusionsToText(records())).toBe([
      "excluded: sut-18x-not-relayed",
      `  ${record.line}`,
      "  evidence: leg 2 msg 1 on '1000-base' — took the 180 (msg 1) and sent no 18x of its own anywhere in the case"
    ].join("\n"))
  })
})
